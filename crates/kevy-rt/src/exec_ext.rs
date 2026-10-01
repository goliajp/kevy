//! Extension messages: payloads a command layer's write hooks address to
//! other shards ([`Commands::take_ext_out`]), applied there by
//! [`Commands::apply_ext`]. kevy's global indexes are the user: a row write
//! on one shard updates the index partition another shard owns.
//!
//! A client's write whose messages went to other shards holds its reply
//! until every one is acknowledged, so a read issued after the reply sees
//! the index as the write left it. Writes nobody waits on — expiry, a
//! replica applying its primary's stream, a backfill — send theirs at the
//! end of the loop iteration, unacknowledged. Messages from one shard to
//! another arrive in the order they were sent, so both kinds stay ordered.

use std::collections::HashMap;

use crate::Commands;
use crate::message::{Agg, Inbound, Part, SmallReply};
use crate::shard::Shard;

/// Replies held until their write's messages are acknowledged.
#[derive(Default)]
pub(crate) struct ExtWaits {
    next: u64,
    waits: HashMap<u64, (u32, Deliver)>,
}

/// Where a held reply goes once its acknowledgements are in.
pub(crate) enum Deliver {
    /// Into a connection on this shard.
    Local { conn: u64, seq: u64, part: Part },
    /// Back to the shard that forwarded the command; `w0` is the durable
    /// watermark the reply must wait for, as the batch it came in had.
    Remote { origin: usize, w0: Option<u64>, conn: u64, seq: u64, part: Part },
}

/// A token to acknowledge, and how many shards will.
pub(crate) type ExtToken = (u64, u32);

impl<C: Commands> Shard<C> {
    /// Send what the hooks queued since the last take. Messages for this
    /// shard apply here and now. With `ack`, the ones sent elsewhere carry
    /// a token to acknowledge, returned when there were any.
    pub(crate) fn send_ext(&mut self, ack: bool) -> Option<ExtToken> {
        let out = self.commands.take_ext_out();
        if out.is_empty() {
            return None;
        }
        // Commands forwarded earlier wait in this iteration's request
        // batches; ship them first, or a message sent now would reach their
        // shard ahead of them and apply out of the client's order (the same
        // reason WAIT flushes before it arms).
        self.flush_requests();
        let remote = out.iter().filter(|(to, _)| *to != self.id).count() as u32;
        let token = if ack && remote > 0 {
            self.ext_waits.next += 1;
            self.ext_waits.next
        } else {
            0
        };
        for (to, payload) in out {
            if to == self.id {
                self.commands.apply_ext(&mut self.store, &payload);
            } else {
                self.send_to(to, Inbound::ExtDelta { from: self.id, token, payload });
            }
        }
        (token != 0).then_some((token, remote))
    }

    /// Run `op` here for `conn`'s command `seq` and fold its part — held
    /// first when the op's hooks sent messages another shard must apply.
    pub(crate) fn exec_local(&mut self, conn: u64, seq: u64, op: crate::message::Op) {
        let part = self.exec_op(op);
        self.fold_unless_held(conn, seq, part);
    }

    /// Fold `part` for `conn`'s command `seq` — held first when that
    /// command's hooks sent messages another shard must apply.
    pub(crate) fn fold_unless_held(&mut self, conn: u64, seq: u64, part: Part) {
        match self.send_ext(true) {
            Some(t) => self.hold_ext(t, Deliver::Local { conn, seq, part }),
            None => self.fold(conn, seq, part),
        }
    }

    /// Reply to a command `origin` forwarded — or hold the reply when its
    /// hooks sent messages another shard must apply first.
    pub(crate) fn reply_forwarded(
        &mut self,
        origin: usize,
        w0: Option<u64>,
        conn: u64,
        seq: u64,
        part: Part,
    ) {
        if let Some(part) = self.part_unless_held(origin, w0, conn, seq, part) {
            self.send_to(origin, Inbound::Response { conn, seq, part });
        }
    }

    /// `part` back when nothing holds it, for the caller to send with the
    /// rest of its batch; `None` when it now waits for acknowledgements.
    pub(crate) fn part_unless_held(
        &mut self,
        origin: usize,
        w0: Option<u64>,
        conn: u64,
        seq: u64,
        part: Part,
    ) -> Option<Part> {
        match self.send_ext(true) {
            Some(t) => {
                self.hold_ext(t, Deliver::Remote { origin, w0, conn, seq, part });
                None
            }
            None => Some(part),
        }
    }

    /// [`Self::on_ext_ack`], then flush the connection a released reply
    /// landed in, the way the inbox flushes a folded response.
    pub(crate) fn on_ext_ack_flush<const DIRECT_FLUSH: bool>(
        &mut self,
        token: u64,
    ) -> std::io::Result<()> {
        if let Some(conn) = self.on_ext_ack(token) {
            if DIRECT_FLUSH {
                self.flush_conn(conn)?;
            } else {
                self.mark_pending_write_dirty(conn);
            }
        }
        Ok(())
    }

    /// Hold `deliver` until the token's acknowledgements are in.
    pub(crate) fn hold_ext(&mut self, (token, remote): ExtToken, deliver: Deliver) {
        self.ext_waits.waits.insert(token, (remote, deliver));
    }

    /// A write replied inline, straight into its connection's output,
    /// before its hooks queued messages that need acknowledging: take the
    /// reply back out and hold it as a pending slot. The connection had no
    /// pending slot (the inline path's precondition), so nothing was
    /// written after it and every later command now queues behind it.
    pub(crate) fn hold_inline_reply(&mut self, conn_id: u64, out_pre_len: usize, token: ExtToken) {
        let Some(conn) = self.conns.get_mut(&conn_id) else { return };
        let reply = conn.output.split_off(out_pre_len);
        conn.next_emit -= 1;
        let seq = conn.next_emit;
        self.push_pending_slot(conn_id, 1, Agg::First(None), false);
        let part = Part::Reply(SmallReply::from_vec(reply));
        self.hold_ext(token, Deliver::Local { conn: conn_id, seq, part });
    }

    /// A message from another shard's hook: apply it, and acknowledge it
    /// when the sender waits.
    pub(crate) fn on_ext_delta(&mut self, from: usize, token: u64, payload: &[u8]) {
        self.commands.apply_ext(&mut self.store, payload);
        if token != 0 {
            self.send_to(from, Inbound::ExtAck { token });
        }
    }

    /// One acknowledgement in; the last one releases the held reply.
    /// Returns the local connection that now has output to flush.
    pub(crate) fn on_ext_ack(&mut self, token: u64) -> Option<u64> {
        let (left, _) = self.ext_waits.waits.get_mut(&token)?;
        *left -= 1;
        if *left > 0 {
            return None;
        }
        match self.ext_waits.waits.remove(&token)?.1 {
            Deliver::Local { conn, seq, part } => {
                self.fold(conn, seq, part);
                Some(conn)
            }
            Deliver::Remote { origin, w0, conn, seq, part } => {
                self.send_or_hold_response(w0, origin, Inbound::Response { conn, seq, part });
                None
            }
        }
    }
}

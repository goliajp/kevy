//! MULTI/EXEC — the server-side transaction face of [`Shard`]: queueing
//! inside MULTI, EXECABORT, and the EXEC dispatch loop with its AOF
//! marker bracket (the queued commands replay all-or-nothing on the
//! connection's shard; cross-shard fan-out is per-shard — see
//! docs/persistence.md). Split from `exec.rs` (500-LOC discipline).

use crate::shard::Shard;
use crate::{Commands, TxnKind};
use kevy_resp::{ArgvView, encode_array_len};

impl<C: Commands> Shard<C> {
    pub(crate) fn handle_txn_state<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        in_multi: bool,
        txn_kind: &TxnKind,
        args: &A,
    ) {
        match (in_multi, txn_kind) {
            (false, TxnKind::Multi) => {
                if let Some(c) = self.conns.get_mut(&conn_id) {
                    c.multi = Some(Vec::new());
                    c.multi_dirty = false;
                }
                self.immediate_reply(conn_id, b"+OK\r\n".to_vec());
            }
            (false, TxnKind::Exec) => {
                self.immediate_reply(conn_id, b"-ERR EXEC without MULTI\r\n".to_vec());
            }
            (false, TxnKind::Discard) => {
                self.immediate_reply(conn_id, b"-ERR DISCARD without MULTI\r\n".to_vec());
            }
            (true, TxnKind::Multi) => {
                self.immediate_reply(conn_id, b"-ERR MULTI calls can not be nested\r\n".to_vec());
            }
            (true, TxnKind::Discard) => {
                // DISCARD drops the queued cmds AND any `WATCH`-ed keys
                // (Redis semantics — see https://redis.io/commands/discard).
                if let Some(c) = self.conns.get_mut(&conn_id) {
                    c.multi = None;
                    c.multi_dirty = false;
                    c.watched.clear();
                }
                self.immediate_reply(conn_id, b"+OK\r\n".to_vec());
            }
            (true, TxnKind::Exec) => self.exec_transaction(conn_id),
            (true, TxnKind::Watch) => self
                .immediate_reply(conn_id, b"-ERR WATCH inside MULTI is not allowed\r\n".to_vec()),
            (true, TxnKind::Other) => self.queue_in_multi(conn_id, args),
            // (false, Other | Watch) dispatched on the early path above.
            (false, TxnKind::Other | TxnKind::Watch) => {}
        }
    }

    /// Queue one command inside an open `MULTI`. Redis validates the
    /// verb + arity at queue time: an unknown verb or too-few args is
    /// answered with the error (not `+QUEUED`) and poisons the
    /// transaction so `EXEC` returns `-EXECABORT`.
    fn queue_in_multi<A: ArgvView + ?Sized>(&mut self, conn_id: u64, args: &A) {
        if let Some(err) = self.commands.queue_error(args) {
            if let Some(c) = self.conns.get_mut(&conn_id) {
                c.multi_dirty = true;
            }
            self.immediate_reply(conn_id, err);
            return;
        }
        if let Some(q) = self.conns.get_mut(&conn_id).and_then(|c| c.multi.as_mut()) {
            q.push(args.to_argv());
        }
        self.immediate_reply(conn_id, b"+QUEUED\r\n".to_vec());
    }

    /// `EXEC` — emit a `*N` array header, then run the queued commands in order.
    /// The seq-ordered ring concatenates their replies into one valid array.
    /// If the conn has any `WATCH`-ed keys, delegate to the pre-check fan-out
    /// path in [`crate::exec_watch`] (aborts if any watched key is dirty).
    /// A command failed to queue (unknown verb / bad arity): abort the
    /// whole transaction, running nothing (Redis EXECABORT). Returns
    /// whether the abort happened.
    fn exec_abort_if_dirty(&mut self, conn_id: u64) -> bool {
        let dirty = self.conns.get(&conn_id).is_some_and(|c| c.multi_dirty);
        if dirty {
            if let Some(c) = self.conns.get_mut(&conn_id) {
                c.multi = None;
                c.multi_dirty = false;
                c.watched.clear();
            }
            self.immediate_reply(
                conn_id,
                b"-EXECABORT Transaction discarded because of previous errors.\r\n".to_vec(),
            );
        }
        dirty
    }

    fn exec_transaction(&mut self, conn_id: u64) {
        if self.exec_abort_if_dirty(conn_id) {
            return;
        }
        let (queued, watched) = match self.conns.get_mut(&conn_id) {
            Some(c) => (c.multi.take().unwrap_or_default(), std::mem::take(&mut c.watched)),
            None => return,
        };
        if !watched.is_empty() {
            self.exec_transaction_watched(conn_id, queued, watched);
            return;
        }
        let mut header = Vec::new();
        encode_array_len(&mut header, queued.len() as i64);
        self.immediate_reply(conn_id, header);
        // EXEC is the real atomic unit: bracket the queued commands'
        // local AOF appends with transaction markers so replay applies
        // them all-or-nothing. (Historically this rode the reactor
        // batch's markers; that window is fsync-only now.) One queued
        // command is atomic by itself — no markers.
        let marked = queued.len() > 1;
        if marked {
            self.aof_begin_group();
        }
        for cmd in &queued {
            let resolved = self.commands.resolve(cmd);
            // EXEC's queued cmds inherit the conn's proto at execution
            // time (same per-cmd capture as the live dispatch path).
            let Some(c) = self.conns.get_mut(&conn_id) else { return };
            let seq = c.next_seq;
            c.next_seq += 1;
            let proto = c.proto;
            // cluster_conn = false: queued transactions execute with full
            // cross-shard fan-out even on a cluster conn (superset
            // behaviour — the redirect already happened, or never will).
            self.start_command(conn_id, seq, proto, cmd, resolved, false);
        }
        if marked {
            self.aof_end_group_logged();
        }
    }
}

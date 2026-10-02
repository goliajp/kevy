//! A connection's later commands wait behind a multi-step cross-shard
//! command — RENAME, COPY, LMOVE, SMOVE, MSETNX, the multi-key pops and
//! the copy-and-place stores — until it is answered.
//!
//! Such a command sends its next step only when the previous one answers,
//! while the commands after it in the same pipeline would go out at once:
//! a `GET` of RENAME's destination would reach that shard before the
//! value does. So the batch stops after the command, the rest stays in
//! the conn's input, and the reactor dispatches it once the reply is out.
//! Single-step fan-outs (MGET, MSET, DEL) need no hold: their requests
//! leave before anything sent after them.

use crate::Commands;
use crate::batch_ends::BatchOutcome;
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    /// Hold `conn_id`'s later commands behind `seq` if it is unanswered —
    /// it is answered already when every step ran on this shard.
    pub(crate) fn hold_if_pending(&mut self, conn_id: u64, seq: u64) {
        let Some(c) = self.conns.get_mut(&conn_id) else { return };
        if c.next_emit > seq {
            return;
        }
        c.hold = Some(seq);
        if !self.held.contains(&conn_id) {
            self.held.push(conn_id);
        }
    }

    /// The held conns whose command has been answered, released; conns
    /// that closed meanwhile are dropped.
    pub(crate) fn take_released(&mut self) -> Vec<u64> {
        let conns = &mut self.conns;
        let mut released = Vec::new();
        self.held.retain(|id| {
            let Some(c) = conns.get_mut(id) else { return false };
            match c.hold {
                Some(seq) if c.next_emit > seq => {
                    c.hold = None;
                    released.push(*id);
                    false
                }
                Some(_) => true,
                None => false,
            }
        });
        released
    }

    /// Dispatch what a released conn's input holds. `None` when the conn
    /// is gone, before or during the batch.
    pub(crate) fn dispatch_held_input(&mut self, conn_id: u64) -> Option<BatchOutcome> {
        let mut input = std::mem::take(&mut self.conns.get_mut(&conn_id)?.input);
        let outcome = self.dispatch_batch(conn_id, &input);
        if outcome.conn_gone {
            return None;
        }
        input.drain(..outcome.consumed);
        self.conns.get_mut(&conn_id)?.input = input;
        Some(outcome)
    }

    /// Whether `conn_id`'s commands are held.
    pub(crate) fn is_held(&self, conn_id: u64) -> bool {
        self.conns.get(&conn_id).is_some_and(|c| c.hold.is_some())
    }
}

//! The one place a mutated key is announced to everything derived from
//! it. Split from `exec_op` for the 500-LOC house rule; the doc comment
//! below is the reason the module exists at all.

use crate::Commands;
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    /// One key was mutated by a cross-shard op: invalidate its WATCHers
    /// and tell the derived structures (secondary indexes) to recompute.
    ///
    /// The two always go together. `Commands::on_write` used to be
    /// called from exactly one place — the single-key dispatch path
    /// (`exec_dispatch::post_write_housekeeping`), which fires only when
    /// the resolver produced a `key_idx`. Every op below routes by key
    /// WITHOUT one (multi-key `DEL`/`UNLINK`, `MSET`, the cross-shard
    /// `RENAME` and `LMOVE` two-steps, the `*STORE` destinations), so
    /// their keys were bumped for WATCH and never reached the index:
    /// `DEL row:7 row:11` left both rows answering `IDX.QUERY` forever
    /// (`IDX.VERIFY` reported the drift; nothing repaired it), which
    /// breaks the derived-by-construction invariant the index rests on.
    /// Pairing them here means the next op that mutates a key gets it
    /// right by using the one helper.
    #[inline]
    pub(crate) fn note_key_mutated(&mut self, key: &[u8]) {
        self.store.bump_if_watched(key);
        self.commands.on_write(&mut self.store, key);
    }
}

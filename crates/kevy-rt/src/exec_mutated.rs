//! The one place a mutated key is announced to everything derived from
//! it. Split from `exec_op` for the 500-LOC house rule; the doc comment
//! below is the reason the module exists at all.

use crate::Commands;
use crate::shard::Shard;
use kevy_resp::ArgvView;

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

    /// The effect of an op that executed on this shard: append it to the
    /// AOF **and** push it to any streaming replica.
    ///
    /// These are the same event, and they were not travelling together.
    /// `exec_op` logged its effect frames and never pushed them, so every
    /// mutation routed as an `Op` — multi-key `DEL`/`UNLINK`, `MSET`, the
    /// cross-shard `RENAME` and `LMOVE` two-steps, the `*STORE`
    /// destinations, `FLUSHALL` — was durable on disk and invisible to
    /// replicas. Measured: single-key `DEL row:2` reaches a replica,
    /// `DEL row:3 row:4` never does, and the replica keeps answering with
    /// rows the primary deleted.
    ///
    /// The push is suppressed while this shard is applying a frame from
    /// its own upstream, exactly as on the dispatch path — a replica
    /// re-emitting what it just applied is how a chain loops.
    pub(crate) fn log_effect<A: ArgvView + ?Sized>(&mut self, args: &A) {
        self.log(args);
        if let Some(src) = self.replicate.as_mut().map(|f| f.source_mut())
            && !crate::replication_gate::is_applying_replicated()
        {
            src.push_mutation(args);
        }
    }
}

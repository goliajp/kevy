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

    /// Record a value this shard just placed under `key`, as the write
    /// commands that reconstruct it — the cross-shard `RENAME`'s
    /// destination half.
    ///
    /// The frames come from the same serializer `BGREWRITEAOF` uses
    /// (`kevy_persist::value_as_v1_frames`), so every `Value` variant,
    /// TTL and stream shape is covered by the implementation that
    /// already has to be right, rather than by a second one written for
    /// this path. V1 framing is plain RESP, so it parses straight back
    /// into the `Argv`s the AOF and the replication stream both take.
    pub(crate) fn log_value_placed(
        &mut self,
        key: &[u8],
        value: &kevy_store::Value,
        ttl_ms: Option<u64>,
    ) {
        if self.aof.is_none() && self.replicate.is_none() {
            return;
        }
        let buf = kevy_persist::value_as_v1_frames(key, value, ttl_ms);
        let mut pos = 0usize;
        let mut argv = kevy_resp::Argv::default();
        while pos < buf.len() {
            argv.clear();
            match kevy_resp::parse_command_into(&buf[pos..], &mut argv) {
                Ok(Some(used)) => {
                    pos += used;
                    self.log_effect(&argv);
                }
                // The serializer's own output not parsing back is a bug
                // in one of the two, not a runtime condition: stop rather
                // than log half a value.
                _ => break,
            }
        }
    }

    /// The source half of a cross-shard `RENAME`, recorded **after** the
    /// destination's put committed — never at take time.
    ///
    /// Take time is too early: a `RENAMENX` whose put is refused rolls
    /// the value back, and a `DEL src` already in the log would then say
    /// a live key was deleted. Late has its own trap, which the
    /// existence check closes: a client can create `src` again between
    /// the take and this call, and its `SET src …` is already in the log
    /// *before* this point — appending a delete after it would replay
    /// away a value that is really there. If the key is back, the
    /// client's own record is the truth and this one is not needed.
    ///
    /// Crash contract, measured rather than assumed: the two halves land
    /// in two different shards' AOFs and are not atomic. Dropping this
    /// record (the window a crash would open) and restarting replays the
    /// key under BOTH names — `src` and `dst` both present, DBSIZE 2.
    /// That is the deliberate direction: a duplicate is recoverable by
    /// hand, a vanished key is not.
    pub(crate) fn log_rename_source_committed(&mut self, src: &[u8]) {
        if self.store.key_exists(src) {
            return;
        }
        let mut c = kevy_resp::Argv::with_capacity(2, 0);
        c.push(b"DEL");
        c.push(src);
        self.log_effect(&c);
    }
}

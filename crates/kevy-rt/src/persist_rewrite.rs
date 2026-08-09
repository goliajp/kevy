//! The two-phase AOF rewrite's completion half on [`Shard`]: applying
//! the worker's `Rewrite` / `TeeAppend` results and driving the tee
//! handoff loop. Split from `persist_worker.rs` at the 500-LOC line —
//! the Aof-side state transitions live in kevy-persist's
//! `aof_rewrite.rs`; this file owns the reactor-side protocol.

use crate::Commands;
use crate::persist_worker::{PersistDone, PersistJob};
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    /// Apply a rewrite-family completion (the `Rewrite` / `TeeAppend`
    /// arms of `commit_persist_done` — `Save` stays there).
    #[cold]
    pub(crate) fn commit_rewrite_done(&mut self, done: PersistDone) {
        match done {
            PersistDone::Rewrite {
                result: Ok(keys),
                tmp,
            } => {
                self.rewrite_handoff = Some((tmp, keys, 0));
                self.advance_rewrite_handoff();
            }
            PersistDone::TeeAppend {
                result: Ok(()),
                tmp: _,
            } => {
                self.advance_rewrite_handoff();
            }
            PersistDone::TeeAppend {
                result: Err(e),
                tmp,
            } => {
                eprintln!("kevy: shard {} aof rewrite tee append failed: {e}", self.id);
                self.rewrite_handoff = None;
                if let Some(aof) = &mut self.aof {
                    aof.abort_concurrent_rewrite();
                }
                let _ = std::fs::remove_file(&tmp);
            }
            PersistDone::Rewrite {
                result: Err(e),
                tmp,
            } => {
                eprintln!("kevy: shard {} aof rewrite failed: {e}", self.id);
                if let Some(aof) = &mut self.aof {
                    aof.abort_concurrent_rewrite();
                }
                let _ = std::fs::remove_file(&tmp);
            }
            // By-argument unreachable (the caller's match keeps Save in
            // commit_persist_done): fall back loudly rather than panic —
            // a dropped Save completion must not tear the shard down.
            PersistDone::Save { .. } => {
                eprintln!(
                    "kevy: shard {} Save completion routed to rewrite arm — dropped",
                    self.id
                );
            }
        }
    }

    /// The two-phase rewrite's driver: hand large tee generations to
    /// the worker (append+fsync off-thread), and only when the current
    /// generation is small do the bounded synchronous swap. The
    /// synchronous cost is the handoff window's writes — ms — instead
    /// of the whole rewrite window's (measured 9.5 s on a firehose).
    /// Iterations are capped: if ingest outruns the disk, the final
    /// swap pays one bounded-large append rather than looping forever.
    #[cold]
    pub(crate) fn advance_rewrite_handoff(&mut self) {
        const SMALL_TEE: usize = 4 << 20; // 4 MiB: ms-scale append+sync
        const MAX_HANDOFFS: u8 = 4;
        let Some((tmp, keys, iters)) = self.rewrite_handoff.take() else {
            return;
        };
        let Some(aof) = &mut self.aof else { return };
        let tee = aof.take_tee_for_handoff().unwrap_or_default();
        if tee.len() > SMALL_TEE && iters < MAX_HANDOFFS {
            if self.persist.submit(
                self.id,
                PersistJob::TeeAppend {
                    tmp: tmp.clone(),
                    bytes: tee,
                },
            ) {
                self.rewrite_handoff = Some((tmp, keys, iters + 1));
            } else {
                // Worker gone — the handed-off tee is lost with it, so
                // the tmp image is incomplete: abort the rewrite. No
                // data is at risk (the live file has carried every one
                // of those writes through the normal append path).
                eprintln!(
                    "kevy: shard {} persist worker unavailable for tee handoff — rewrite aborted",
                    self.id
                );
                self.aof
                    .as_mut()
                    .expect("checked above")
                    .abort_concurrent_rewrite();
                let _ = std::fs::remove_file(&tmp);
            }
            return;
        }
        let aof = self.aof.as_mut().expect("checked above");
        if let Err(e) = aof.finish_concurrent_rewrite_with(&tmp, keys, tee) {
            eprintln!("kevy: shard {} aof rewrite swap failed: {e}", self.id);
            aof.abort_concurrent_rewrite();
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

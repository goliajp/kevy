//! v1.25 A.2/A.3 bio-drop batching: heavy `Value`s displaced by SET
//! overwrites are shipped to the runtime's bio thread in per-iteration
//! batches instead of dropping inline on the reactor. Split out of
//! `lib.rs` to keep it under the 500-LOC house cap; verbatim from
//! before the move.

use crate::value::{self, Value};
use crate::Store;

/// Maximum [`Store::pending_drops`] depth before forcing a flush
/// inside `maybe_offload_drop` (rather than waiting for the reactor's
/// per-iter `flush_pending_drops`). Caps memory held in the batch
/// buffer at ≤ 64 × sizeof(Box<Value>) (≤ 512 B of pointers + whatever
/// the boxed payloads weigh — which we WANT to ship anyway, since
/// holding the bio-bound batch defeats the point of off-reactor frees).
/// 64 picked as: amortises mpsc send cost (~few hundred ns) across
/// enough drops that per-drop overhead is ≤ 10 ns, while staying small
/// enough that worst-case bunch-up latency at the bio thread is bounded.
pub(crate) const MAX_PENDING_DROPS: usize = 64;

impl Store {
    /// Install the runtime's bio-drop channel (v1.25 A.3 + A.2). Called
    /// once from `kevy-rt::Runtime::run` per shard before the reactor
    /// loop starts. After install, [`Self::maybe_offload_drop`] (invoked
    /// from the SET overwrite fast path) accumulates oversize `Value`s
    /// into a per-shard batch; the reactor calls
    /// [`Self::flush_pending_drops`] at the end of every iter to ship
    /// the batch in one mpsc send. Bounded the Axis I 10 KB SET p999/max
    /// blow-up that synchronous `Box::<[u8]>::drop` of a jemalloc
    /// large-class slot caused (see `kevy_rt::bio`).
    #[inline]
    pub fn set_bio_drop_sender(&mut self, sender: value::BioDropSender) {
        self.bio_drop_sender = Some(sender);
    }

    /// Accumulate `old` into the per-shard bio-drop batch buffer
    /// ([`Store::pending_drops`]) if it's heap-heavy AND a bio channel
    /// is installed. Otherwise drop inline. The hot path is one branch
    /// on `bio_drop_sender.is_none()` followed by the variant-cheap
    /// [`Value::is_heap_heavy`] check; for the `Value::Str(SmallBytes)`
    /// steady state of typical bench shapes the inline-drop path is
    /// preserved unchanged.
    ///
    /// **v1.25 A.2 batch model**: per-send mpsc cost (atomic +
    /// cross-thread cacheline) is amortised across the batch by
    /// [`Self::flush_pending_drops`], which the reactor calls once per
    /// iter. Force-flushes here when the buffer hits
    /// [`MAX_PENDING_DROPS`] to bound RAM in-flight.
    #[inline]
    pub(crate) fn maybe_offload_drop(&mut self, old: Value) {
        if self.bio_drop_sender.is_none() {
            // No channel (bare Store / embedded reaper / tests): the
            // Value falls out of scope and drops inline. Same
            // behaviour as v1.24.
            drop(old);
            return;
        }
        if !old.is_heap_heavy() {
            // Under-threshold: jemalloc small-class free is sub-µs.
            // The Vec::push + force-flush branch costs more than the
            // inline free for this size — leave it inline.
            drop(old);
            return;
        }
        self.pending_drops.push(old);
        if self.pending_drops.len() >= MAX_PENDING_DROPS {
            self.flush_pending_drops();
        }
    }

    /// Ship the per-shard bio-drop batch buffer to the bio thread in
    /// one mpsc send. Called from `kevy-rt`'s reactor loop at the end
    /// of every iteration (both the epoll `Shard::run` and the io_uring
    /// `Shard::run_uring` paths, just before the AOF fsync window so a
    /// pending fsync stall doesn't pin a batch-ful of heavy values in
    /// per-shard memory).
    ///
    /// Empty-buffer fast path: zero work, predictable not-taken
    /// branch. Reactor calls this unconditionally per iter; the steady-
    /// state cost for a no-SET-overwrite iter is one length check.
    ///
    /// `SendError` here means the bio thread has exited (shutdown
    /// territory — `Runtime::run` has dropped its sender AFTER the
    /// shard threads joined). Drop the batch inline; the `SendError`
    /// payload carries the `Vec` back so its `Box<Value>`s run their
    /// Drop here, preserving correctness.
    #[inline]
    pub fn flush_pending_drops(&mut self) {
        if self.pending_drops.is_empty() {
            return;
        }
        let tx = match self.bio_drop_sender.as_ref() {
            Some(tx) => tx,
            // Shouldn't happen — caller (`maybe_offload_drop`) only
            // pushes when the sender exists. Defensive: if a future
            // refactor invokes `flush_pending_drops` from somewhere
            // unconditional, drop the batch inline.
            None => {
                self.pending_drops.clear();
                return;
            }
        };
        let batch = std::mem::take(&mut self.pending_drops);
        if let Err(_send_err) = tx.send(batch) {
            // Bio thread is gone (shutdown). The SendError carries
            // the Vec, which drops here — every Box<Value> runs its
            // Drop inline. Benign one-time stall during tear-down.
        }
    }
}

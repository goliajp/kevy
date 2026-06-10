//! A recycling pool of owned [`Argv`]s for the cross-shard forward path.
//!
//! Forwarding a command to its owning shard materialises an owned `Argv`
//! (2 mallocs) on the origin core, which the owning core then frees after
//! dispatch — a cross-thread alloc/free pair per forwarded command. The
//! pool breaks that cycle: the origin fills a recycled `Argv` (capacity
//! retained → no malloc), the owning shard executes it borrowed, and the
//! spent husk rides back to the origin with the reply batch, where it
//! re-enters the origin's pool. Returning to the *origin* (rather than
//! pooling at the owner) keeps each shard's pool level matched to its own
//! conn demand by construction — recycle-at-the-owner measurably starved
//! conn-heavy shards (accept skew) while overfilling quiet ones, leaving
//! the malloc rate unchanged.

use crate::argv::Argv;
use crate::argv_view::ArgvView;

/// Most argvs a pool retains. With husks returning to their origin the
/// steady-state pool level equals the shard's own forwarded in-flight
/// (conns × pipeline depth — ≈1600 at the 50-conn × P256 bench corner),
/// so the cap is headroom, not a working limit; it only bounds memory
/// when a conn burst comes and goes. 8192 ≈ ~1 MiB worst case per shard
/// at typical small-argv buffer sizes.
const MAX_POOLED: usize = 8192;

/// Largest arg-bytes buffer worth retaining. A one-off `SET k <1 MB>`
/// must not park a megabyte in the pool forever; oversized husks are
/// dropped instead of pooled.
const MAX_POOLED_BYTES: usize = 4096;

/// A recycling pool of owned [`Argv`]s. See the module docs for the
/// cross-shard ownership cycle it serves.
#[derive(Default)]
pub struct ArgvPool {
    free: Vec<Argv>,
}

impl ArgvPool {
    /// An empty pool.
    pub fn new() -> Self {
        Self::default()
    }

    /// An owned `Argv` filled with `view`'s arguments — a recycled one
    /// (no malloc in steady state) when the pool has a husk, else fresh.
    pub fn take_filled<A: ArgvView + ?Sized>(&mut self, view: &A) -> Argv {
        let mut argv = self.free.pop().unwrap_or_default();
        view.copy_into(&mut argv);
        argv
    }

    /// Recycle a spent `Argv`. Dropped instead of pooled when the pool is
    /// full or the argv's buffer is oversized (retention policy — see
    /// [`MAX_POOLED`] / [`MAX_POOLED_BYTES`]).
    pub fn put(&mut self, argv: Argv) {
        if self.free.len() < MAX_POOLED && argv.buf_capacity() <= MAX_POOLED_BYTES {
            self.free.push(argv);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv_of(args: &[&[u8]]) -> Argv {
        let mut a = Argv::default();
        for arg in args {
            a.push(arg);
        }
        a
    }

    #[test]
    fn take_filled_matches_to_argv() {
        let mut pool = ArgvPool::new();
        let src = argv_of(&[b"SET", b"k", b"v"]);
        let got = pool.take_filled(&src);
        assert_eq!(got, src.to_argv());
    }

    #[test]
    fn recycled_argv_is_refilled_clean() {
        let mut pool = ArgvPool::new();
        pool.put(argv_of(&[b"SET", b"stale-key", b"stale-value"]));
        let src = argv_of(&[b"GET", b"k"]);
        let got = pool.take_filled(&src);
        assert_eq!(got.len(), 2);
        assert_eq!(got.get(0), Some(b"GET" as &[u8]));
        assert_eq!(got.get(1), Some(b"k" as &[u8]));
    }

    #[test]
    fn pool_count_is_capped() {
        let mut pool = ArgvPool::new();
        for _ in 0..(MAX_POOLED + 100) {
            pool.put(argv_of(&[b"GET", b"k"]));
        }
        assert_eq!(pool.free.len(), MAX_POOLED);
    }

    #[test]
    fn oversized_buffers_are_not_retained() {
        let mut pool = ArgvPool::new();
        let big = vec![b'x'; MAX_POOLED_BYTES + 1];
        pool.put(argv_of(&[b"SET", b"k", &big]));
        assert!(pool.free.is_empty());
    }

    #[test]
    fn copy_into_reuses_capacity() {
        let src = argv_of(&[b"SET", b"key", b"value"]);
        let mut out = Argv::default();
        src.copy_into(&mut out);
        let cap = out.buf_capacity();
        // Refill with same-size content: capacity must not have shrunk
        // (the whole point of recycling).
        src.copy_into(&mut out);
        assert_eq!(out.buf_capacity(), cap);
        assert_eq!(out, src);
    }
}

//! v2.4 — embedded blocking pops (`blpop` / `brpop` / `bzpopmin`).
//!
//! Design (the park-wait note, per ROADMAP):
//! - One process-wide [`Blocker`] per store: a wake **generation**
//!   under a mutex + condvar, plus an atomic waiter count.
//! - **Writers** (every `commit_write`) check `waiters` with a single
//!   Relaxed load — zero cost while nobody blocks. When waiters exist,
//!   bump the generation + `notify_all`. Any write wakes every
//!   waiter; each re-polls its own keys (spurious wakeups are cheap
//!   re-polls — correctness over per-key bookkeeping at embedded
//!   contention levels).
//! - **Waiters** loop: non-blocking poll across their keys → if
//!   empty, wait on the condvar with the remaining deadline, keyed to
//!   the generation observed *before* the poll — the classic
//!   recheck-after-wait pattern, so a push landing between the poll
//!   and the wait is never lost.
//! - `timeout = None` blocks indefinitely (matches `BLPOP key 0`).
//!
//! Fairness: multiple blocked consumers re-poll under their shard
//! write locks; the lock queue arbitrates. No FIFO ticket order is
//! promised (same as the server's cross-shard arbiter).

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::store::Store;

/// `(key, member, score)` from a zset blocking pop.
type ZPopHit = (Vec<u8>, Vec<u8>, f64);

/// Process-wide wake channel for blocking pops.
pub(crate) struct Blocker {
    waiters: AtomicUsize,
    generation: Mutex<u64>,
    cv: Condvar,
}

impl Blocker {
    pub(crate) fn new() -> Self {
        Self {
            waiters: AtomicUsize::new(0),
            generation: Mutex::new(0),
            cv: Condvar::new(),
        }
    }

    /// Writer side — called from `commit_write`. One Relaxed load when
    /// idle; lock + notify only with live waiters.
    #[inline]
    pub(crate) fn wake_all(&self) {
        if self.waiters.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut g = self.generation.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *g += 1;
        self.cv.notify_all();
    }

    /// Current generation (observe BEFORE polling, wait against it).
    fn generation(&self) -> u64 {
        *self.generation.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Park until the generation moves past `seen` or `deadline`
    /// passes. Returns `false` on timeout.
    fn wait_past(&self, seen: u64, deadline: Option<Instant>) -> bool {
        self.waiters.fetch_add(1, Ordering::Relaxed);
        let mut g = self.generation.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let ok = loop {
            if *g != seen {
                break true;
            }
            match deadline {
                None => {
                    g = self
                        .cv
                        .wait(g)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        break false;
                    }
                    let (guard, res) = self
                        .cv
                        .wait_timeout(g, d - now)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    g = guard;
                    if res.timed_out() && *g == seen {
                        break false;
                    }
                }
            }
        };
        drop(g);
        self.waiters.fetch_sub(1, Ordering::Relaxed);
        ok
    }
}

impl Store {
    /// `BLPOP` — block until one of `keys` has a head element (checked
    /// in argument order each round) or `timeout` passes (`None` =
    /// wait forever). Returns `(key, value)`.
    pub fn blpop(
        &self,
        keys: &[&[u8]],
        timeout: Option<Duration>,
    ) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
        self.block_on(keys, timeout, |s, k| {
            Ok(s.lpop(k, 1)?.into_iter().next().map(|v| (k.to_vec(), v)))
        })
    }

    /// `BRPOP` — tail-end counterpart of [`Self::blpop`].
    pub fn brpop(
        &self,
        keys: &[&[u8]],
        timeout: Option<Duration>,
    ) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
        self.block_on(keys, timeout, |s, k| {
            Ok(s.rpop(k, 1)?.into_iter().next().map(|v| (k.to_vec(), v)))
        })
    }

    /// `BZPOPMIN` — block until one of `keys` has a zset member;
    /// returns `(key, member, score)`.
    pub fn bzpopmin(
        &self,
        keys: &[&[u8]],
        timeout: Option<Duration>,
    ) -> io::Result<Option<ZPopHit>> {
        self.block_on(keys, timeout, |s, k| {
            Ok(s
                .zpopmin(k, 1)?
                .into_iter()
                .next()
                .map(|(m, sc)| (k.to_vec(), m, sc)))
        })
    }

    /// The shared park-wait loop: `try_pop` is the non-blocking probe
    /// run against each key in order, every wake round.
    fn block_on<T>(
        &self,
        keys: &[&[u8]],
        timeout: Option<Duration>,
        try_pop: impl Fn(&Self, &[u8]) -> io::Result<Option<T>>,
    ) -> io::Result<Option<T>> {
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            let seen = self.blocker.generation();
            for k in keys {
                if let Some(hit) = try_pop(self, k)? {
                    return Ok(Some(hit));
                }
            }
            if !self.blocker.wait_past(seen, deadline) {
                return Ok(None); // timed out
            }
        }
    }
}

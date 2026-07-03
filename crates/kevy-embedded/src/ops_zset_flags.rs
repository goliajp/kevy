//! `ZADD` condition-flag surfaces (Redis 6.2 `NX`/`XX`/`GT`/`LT`/`CH`
//! + the `INCR` form). The plain `zadd` hot path is untouched.
//!
//! AOF form: the **effect** is logged, never the condition — applied
//! pairs go into the log as plain unconditional `ZADD` (and the
//! `INCR` form as `ZADD key <new-absolute-score> member`). A
//! conditional verb replayed against divergent state (a replica
//! applying frames onto snapshot-loaded state) could veto differently
//! and diverge; the absolute form is deterministic. Same lesson as
//! v2.0.21's SPOP→SREM propagation fix.

use std::io;

use kevy_store::{ZaddFlags, ZaddReport};

#[cfg(not(target_arch = "wasm32"))]
use crate::replica_glue::ensure_writable;
use crate::store::{Store, commit_write, store_err};

#[cfg(target_arch = "wasm32")]
fn ensure_writable(_s: &Store) -> io::Result<()> {
    Ok(())
}

fn reject_invalid(flags: ZaddFlags) -> io::Result<()> {
    if flags.valid() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "GT, LT, and/or NX options at the same time are not compatible",
        ))
    }
}

impl Store {
    /// Flags-aware `ZADD`. See [`ZaddFlags`]; read
    /// [`ZaddReport::changed`] for the `CH` reply shape. The
    /// monotonic-heal idiom is `zadd_flags(k, pairs, ZaddFlags { gt:
    /// true, ..Default::default() })`.
    pub fn zadd_flags(
        &self,
        key: &[u8],
        pairs: &[(f64, &[u8])],
        flags: ZaddFlags,
    ) -> io::Result<ZaddReport> {
        reject_invalid(flags)?;
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let rep = g
            .store
            .zadd_flags_borrowed(key, pairs, flags)
            .map_err(store_err)?;
        if !rep.applied.is_empty() {
            let score_strs: Vec<Vec<u8>> = rep
                .applied
                .iter()
                .map(|(s, _)| format!("{s}").into_bytes())
                .collect();
            let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + rep.applied.len() * 2);
            parts.push(b"ZADD");
            parts.push(key);
            for (i, (_, m)) in rep.applied.iter().enumerate() {
                parts.push(&score_strs[i]);
                parts.push(m);
            }
            commit_write(&mut g, &parts)?;
        }
        Ok(rep)
    }

    /// `ZADD … INCR` — a conditional `ZINCRBY`; `None` when the flags
    /// veto the operation.
    pub fn zadd_incr(
        &self,
        key: &[u8],
        delta: f64,
        member: &[u8],
        flags: ZaddFlags,
    ) -> io::Result<Option<f64>> {
        reject_invalid(flags)?;
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let next = g
            .store
            .zadd_incr(key, delta, member, flags)
            .map_err(store_err)?;
        if let Some(n) = next {
            let s = format!("{n}");
            commit_write(&mut g, &[b"ZADD", key, s.as_bytes(), member])?;
        }
        Ok(next)
    }
}

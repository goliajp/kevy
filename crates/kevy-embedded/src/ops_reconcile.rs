//! Reconcile derived keys against the rows that imply them.
//!
//! A consumer asked for this: they had to write boot-time verification
//! that rebuilds every derived key from the rows and diffs, and observed
//! that everyone maintaining link or claim keys needs exactly the same
//! thing and will write it slightly differently and slightly wrong.
//!
//! The recipe it checks is `docs/cookbook.md` §21 — derived state as a
//! pure function of the row. Pass that same function here and the
//! checker is the function; there is nothing to keep in step.
//!
//! Two things a hand-rolled version usually gets wrong, both handled
//! here by construction:
//!
//! 1. **It scans the live keyspace**, so a write landing mid-scan is
//!    reported as drift. This runs against a [`Snapshot`], which is
//!    frozen under every shard lock, so a clean system is always clean.
//! 2. **It only looks for missing keys.** A claim left behind by a
//!    half-applied update is an orphan, not an absence, and it is the
//!    failure that silently blocks a later insert. This diffs both ways.

use crate::ops_snapshot_view::Snapshot;
use std::collections::HashSet;

/// How many example keys each direction of the diff carries. Counts are
/// exact regardless; the samples exist to be actionable in a log line,
/// not to be a second copy of the keyspace.
const MAX_SAMPLES: usize = 1000;

/// The result of [`Snapshot::reconcile`].
#[derive(Debug, Default, Clone)]
pub struct ReconcileReport {
    /// Rows visited under the row prefix.
    pub rows: u64,
    /// Distinct derived keys the rows imply.
    pub expected: u64,
    /// Implied by a row, absent from the store — lost derived state.
    /// Exact count; `missing` holds up to [`MAX_SAMPLES`] examples.
    pub missing_count: u64,
    /// Example missing keys.
    pub missing: Vec<Vec<u8>>,
    /// Present under a derived prefix but implied by no row — a claim
    /// or link left behind. Exact count; `orphaned` holds examples.
    pub orphaned_count: u64,
    /// Example orphaned keys.
    pub orphaned: Vec<Vec<u8>>,
}

impl ReconcileReport {
    /// Whether the rows and their derived keys agree exactly.
    pub fn is_clean(&self) -> bool {
        self.missing_count == 0 && self.orphaned_count == 0
    }

    /// Whether the sample lists were cut short — the counts are still
    /// exact, so a caller logging only samples should say so.
    pub fn truncated(&self) -> bool {
        self.missing.len() as u64 != self.missing_count
            || self.orphaned.len() as u64 != self.orphaned_count
    }
}

impl Snapshot {
    /// Rebuild every derived key from the rows under `rows_prefix` and
    /// diff it against what actually exists under `derived_prefixes`.
    ///
    /// `derived` is the function from a row to the keys it implies —
    /// the same one the write path uses (cookbook §21). Whatever it
    /// returns for a row is what that row is entitled to; anything else
    /// under `derived_prefixes` is an orphan.
    ///
    /// `derived_prefixes` must cover everywhere derived keys live. A
    /// derived key outside them is invisible to both directions of the
    /// diff: it cannot be reported missing (nothing looks for it) and
    /// it cannot be reported orphaned (nothing enumerates it). This is
    /// the one way to get a falsely clean report, so the prefixes are a
    /// required argument rather than an option with a default.
    ///
    /// The snapshot is frozen, so a clean system reports clean no
    /// matter what writers are doing.
    ///
    /// ```no_run
    /// # use kevy_embedded::{Store, Config};
    /// # let store = Store::open(Config::default()).unwrap();
    /// let report = store.snapshot().reconcile(
    ///     b"user:",
    ///     &[b"email:", b"dept:"],
    ///     |key, _row| vec![[b"email:".as_slice(), key].concat()],
    /// );
    /// assert!(report.is_clean());
    /// ```
    pub fn reconcile(
        &self,
        rows_prefix: &[u8],
        derived_prefixes: &[&[u8]],
        mut derived: impl FnMut(&[u8], &kevy_store::Value) -> Vec<Vec<u8>>,
    ) -> ReconcileReport {
        let mut report = ReconcileReport::default();
        let mut expected: HashSet<Vec<u8>> = HashSet::new();
        self.each_prefix(rows_prefix, |k, v, _ttl| {
            report.rows += 1;
            expected.extend(derived(k, v));
        });
        report.expected = expected.len() as u64;

        let mut present: HashSet<Vec<u8>> = HashSet::new();
        for p in derived_prefixes {
            self.each_prefix(p, |k, _v, _ttl| {
                present.insert(k.to_vec());
            });
        }

        collect(expected.difference(&present), &mut report.missing_count, &mut report.missing);
        collect(present.difference(&expected), &mut report.orphaned_count, &mut report.orphaned);
        report
    }
}

/// Count everything, keep the first [`MAX_SAMPLES`].
fn collect<'a>(
    keys: impl Iterator<Item = &'a Vec<u8>>,
    count: &mut u64,
    samples: &mut Vec<Vec<u8>>,
) {
    for k in keys {
        *count += 1;
        if samples.len() < MAX_SAMPLES {
            samples.push(k.clone());
        }
    }
}

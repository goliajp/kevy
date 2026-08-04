//! The observation half of the auto-declaration loop (the R4b
//! autodeclare RFC): a refused query IS a complete
//! declaration specification — the name convention carries the table
//! and column, the verb and shape carry the kind, the catalog
//! carries the type. This module is the pure structure both engine
//! faces share: a bounded log written only on the refusal path, and
//! the advice renderer that turns its entries into executable
//! declaration commands. Locking belongs to the caller (the server
//! wraps it in a mutex, the embedded store in its shard lock) — this
//! crate stays synchronization-free like the rest of kevy-index.

use crate::catalog::ValType;
use crate::table::TableCatalog;

/// What shape of query was refused — the kind half of the derived
/// declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdviseShape {
    /// `RANGE`/`EQ` on an undeclared single-column path.
    Range,
    /// `WHERE` naming these columns, in clause order, on a name with
    /// no composite path.
    Where(Vec<Vec<u8>>),
    /// `MATCH` on an undeclared text path.
    Match,
    /// A `FILTER` naming a field the (existing) path does not store.
    Filter(Vec<u8>),
}

/// One observed refusal family: a (name, shape) pair, how often it
/// was refused, and the first argv seen (the human-readable sample).
/// `Clone` so a caller holding the log under a lock can snapshot
/// entries out and render them lock-free.
#[derive(Debug, Clone)]
pub struct AdviseEntry {
    /// The access-path name the query asked for (`<table>.<suffix>`).
    pub name: Vec<u8>,
    /// The refused shape.
    pub shape: AdviseShape,
    /// Refusals observed for this family.
    pub count: u64,
    /// The first refused argv, verbatim.
    pub sample: Vec<Vec<u8>>,
}

/// The bounded refusal log. Insertion-ordered; when full, the entry
/// with the SMALLEST count makes room (a family that keeps being
/// refused defends its seat).
#[derive(Debug)]
pub struct AdviseLog {
    entries: Vec<AdviseEntry>,
    cap: usize,
}

/// [`AdviseLog::new`] — NOT an all-zeroes log: a derived default
/// would set `cap = 0`, which `with_cap` clamps away for a reason.
impl Default for AdviseLog {
    fn default() -> Self {
        Self::new()
    }
}

/// How many refusal families the default log retains.
pub const ADVISE_CAP: usize = 128;

impl AdviseLog {
    /// An empty log with the default capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::with_cap(ADVISE_CAP)
    }

    /// An empty log retaining at most `cap` families.
    #[must_use]
    pub fn with_cap(cap: usize) -> Self {
        Self { entries: Vec::new(), cap: cap.max(1) }
    }

    /// Record one refusal. Families deduplicate on (name, shape); a
    /// full log evicts its least-refused family for a NEW one (an
    /// existing family always just counts).
    pub fn observe(&mut self, name: &[u8], shape: AdviseShape, argv: &[Vec<u8>]) {
        if let Some(e) =
            self.entries.iter_mut().find(|e| e.name == name && e.shape == shape)
        {
            e.count += 1;
            return;
        }
        if self.entries.len() >= self.cap {
            let (weakest, _) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.count)
                .expect("cap >= 1, so a full log is non-empty");
            self.entries.swap_remove(weakest);
        }
        self.entries.push(AdviseEntry {
            name: name.to_vec(),
            shape,
            count: 1,
            sample: argv.to_vec(),
        });
    }

    /// The observed families, most-refused first.
    #[must_use]
    pub fn entries(&self) -> Vec<&AdviseEntry> {
        let mut v: Vec<&AdviseEntry> = self.entries.iter().collect();
        v.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
        v
    }

    /// Forget everything (declarations landed; the slate restarts).
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// One declared path's usage counters — the refusal log's dual: that
/// log says what is missing, this says what goes unused (the reclaim
/// face's raw material). Plain relaxed atomics — the served-query
/// path pays two uncontended stores, never a lock.
#[derive(Debug, Default)]
pub struct UsageCell {
    /// Queries served through this path.
    pub hits: std::sync::atomic::AtomicU64,
    /// Unix seconds of the most recent hit (0 = never).
    pub last_hit_s: std::sync::atomic::AtomicI64,
    /// Unix seconds the path was first seen declared — "never hit"
    /// is only meaningful with an age next to it (a path declared
    /// five seconds ago is not reclaim material).
    pub declared_s: std::sync::atomic::AtomicI64,
}

impl UsageCell {
    /// A fresh cell for a path first seen declared at `now_s`.
    #[must_use]
    pub fn declared_at(now_s: i64) -> Self {
        let c = Self::default();
        c.declared_s.store(now_s, std::sync::atomic::Ordering::Relaxed);
        c
    }

    /// Count one served query at `now_s` (unix seconds).
    pub fn hit(&self, now_s: i64) {
        use std::sync::atomic::Ordering::Relaxed;
        self.hits.fetch_add(1, Relaxed);
        self.last_hit_s.store(now_s, Relaxed);
    }

    /// `(hits, last_hit_s, declared_s)` snapshot.
    #[must_use]
    pub fn read(&self) -> (u64, i64, i64) {
        use std::sync::atomic::Ordering::Relaxed;
        (self.hits.load(Relaxed), self.last_hit_s.load(Relaxed), self.declared_s.load(Relaxed))
    }
}

/// Render one observed family as the declaration command that would
/// have served it — executable verbatim, or `None` when the catalog
/// cannot ground it (unknown table / column: the query itself was
/// malformed, not under-declared).
#[must_use]
pub fn advice_of(e: &AdviseEntry, cat: &TableCatalog) -> Option<String> {
    let dot = e.name.iter().position(|&b| b == b'.')?;
    let (table, suffix) = (&e.name[..dot], &e.name[dot + 1..]);
    let t = cat.get(table)?;
    let show = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    match &e.shape {
        AdviseShape::Range => {
            let ty = t.column_type(suffix)?;
            Some(format!(
                "TABLE.DECLARE {} … INDEX {} range  (column type {})",
                show(table),
                show(suffix),
                ty.tag()
            ))
        }
        AdviseShape::Where(cols) => {
            for c in cols {
                t.column_type(c)?;
            }
            let list =
                cols.iter().map(|c| show(c)).collect::<Vec<_>>().join(" THEN ");
            Some(format!(
                "TABLE.DECLARE {} … ORDERPATH {} ON {}",
                show(table),
                show(suffix),
                list
            ))
        }
        AdviseShape::Match => {
            let ty = t.column_type(suffix)?;
            (ty == ValType::Str).then(|| {
                format!(
                    "IDX.CREATE {} ON PREFIX {} FIELD {} TYPE str KIND text",
                    show(&e.name),
                    show(&t.prefix),
                    show(suffix)
                )
            })
        }
        AdviseShape::Filter(field) => {
            let ty = t.column_type(field)?;
            Some(format!(
                "add VALUES {} (type {}) to the {} declaration",
                show(field),
                ty.tag(),
                show(&e.name)
            ))
        }
    }
}

#[cfg(test)]
#[path = "advise_tests.rs"]
mod tests;

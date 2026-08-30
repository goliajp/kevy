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
use crate::table::{TableCatalog, TableSpec};

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

/// Refusals a family must accumulate before the auto loop declares
/// for it (tables that opted in via `AUTODECLARE <n>`). Empirical
/// knob, not a derivation.
pub const AUTODECLARE_AFTER: u64 = 16;

/// Apply one refused family to the table's declaration — the auto
/// half of the loop, shared by both faces so they cannot derive
/// different declarations. Returns the ledger entry recorded in
/// `auto_added` (`path` or `path#field`); `None` when the budget is
/// spent, the shape is not one a table declaration serves (`MATCH`
/// stays advise-only — a text index carries knobs the loop must not
/// pick), the name does not ground, or it is already declared.
pub fn apply_auto(spec: &mut TableSpec, e: &AdviseEntry) -> Option<Vec<u8>> {
    if spec.auto_added.len() >= spec.autodeclare {
        return None;
    }
    let dot = e.name.iter().position(|&b| b == b'.')?;
    let (table, suffix) = (&e.name[..dot], &e.name[dot + 1..]);
    if table != spec.name {
        return None;
    }
    let entry = auto_entry(spec, &e.name, suffix, &e.shape)?;
    spec.auto_added.push(entry.clone());
    Some(entry)
}

/// [`apply_auto`]'s shape half: mutate the declaration and return the
/// ledger entry, or `None` when the shape does not ground.
fn auto_entry(
    spec: &mut TableSpec,
    name: &[u8],
    suffix: &[u8],
    shape: &AdviseShape,
) -> Option<Vec<u8>> {
    match shape {
        AdviseShape::Range => {
            spec.column_type(suffix)?;
            if spec.indexes.iter().any(|ix| ix.column == suffix) {
                return None;
            }
            spec.indexes.push(crate::table::TableIndex {
                column: suffix.to_vec(),
                kind: crate::IndexKind::Range,
                values: Vec::new(),
            });
            Some(name.to_vec())
        }
        AdviseShape::Where(cols) => {
            for c in cols {
                spec.column_type(c)?;
            }
            if cols.is_empty() || spec.orderpaths.iter().any(|op| op.name == suffix) {
                return None;
            }
            spec.orderpaths.push(crate::table::OrderPath {
                name: suffix.to_vec(),
                on: cols.iter().map(|c| (c.clone(), false)).collect(),
            });
            Some(name.to_vec())
        }
        AdviseShape::Filter(field) => {
            spec.column_type(field)?;
            let ix = spec.indexes.iter_mut().find(|ix| ix.column == suffix)?;
            if ix.values.iter().any(|v| v == field) {
                return None;
            }
            ix.values.push(field.clone());
            let mut entry = name.to_vec();
            entry.push(b'#');
            entry.extend_from_slice(field);
            Some(entry)
        }
        AdviseShape::Match => None,
    }
}

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

    /// Record one refusal, returning the family's count AFTER this
    /// observation — the auto loop's threshold input. Families
    /// deduplicate on (name, shape); a full log evicts its
    /// least-refused family for a NEW one (an existing family always
    /// just counts).
    pub fn observe(&mut self, name: &[u8], shape: AdviseShape, argv: &[Vec<u8>]) -> u64 {
        if let Some(e) = self.entries.iter_mut().find(|e| e.name == name && e.shape == shape) {
            e.count += 1;
            return e.count;
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
        1
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
#[derive(Debug)]
pub struct UsageCell {
    /// Queries served through this path.
    pub hits: std::sync::atomic::AtomicU64,
    /// Unix seconds of the most recent hit (0 = never).
    pub last_hit_s: std::sync::atomic::AtomicI64,
    /// Unix seconds the path was first seen declared — "never hit"
    /// is only meaningful with an age next to it (a path declared
    /// five seconds ago is not reclaim material).
    pub declared_s: std::sync::atomic::AtomicI64,
    /// Windowed paths only: the smallest `lower_bound - boundary`
    /// any query has probed (`i64::MAX` = never observed). A margin
    /// that never goes non-positive means no query has touched the
    /// cold side — the window-narrowing advice's whole input, no max
    /// tracking needed (boundary ≈ max − span, within a bucket).
    pub min_margin: std::sync::atomic::AtomicI64,
}

/// A zeroed cell with the margin UNOBSERVED (`i64::MAX`) — a derived
/// all-zeroes default would read as "a query probed margin 0".
impl Default for UsageCell {
    fn default() -> Self {
        use std::sync::atomic::{AtomicI64, AtomicU64};
        Self {
            hits: AtomicU64::new(0),
            last_hit_s: AtomicI64::new(0),
            declared_s: AtomicI64::new(0),
            min_margin: AtomicI64::new(i64::MAX),
        }
    }
}

impl UsageCell {
    /// A fresh cell for a path first seen declared at `now_s`.
    #[must_use]
    pub fn declared_at(now_s: i64) -> Self {
        let c = Self::default();
        c.declared_s.store(now_s, std::sync::atomic::Ordering::Relaxed);
        c
    }

    /// Record one windowed query's probe depth (`lower - boundary`).
    pub fn probe(&self, margin: i64) {
        self.min_margin.fetch_min(margin, std::sync::atomic::Ordering::Relaxed);
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

/// The window-narrowing advice for one windowed table, given the
/// smallest probe margin any query on one of its paths has shown:
/// `Some` when every observed query kept more than a bucket of
/// margin, so SPAN can shrink by the bucket-aligned amount.
/// Advise-only — the window synthesis point; the engine never
/// narrows on its own.
#[must_use]
pub fn narrow_advice(spec: &TableSpec, margin: i64) -> Option<String> {
    let w = spec.window.as_ref()?;
    if margin == i64::MAX || w.bucket <= 0 {
        return None;
    }
    let narrow = margin - margin.rem_euclid(w.bucket);
    if narrow <= 0 {
        return None;
    }
    let new_span = (w.span - narrow).max(w.bucket);
    if new_span >= w.span {
        return None;
    }
    Some(format!(
        "WINDOW {} SPAN {} — every observed query kept a margin of {}; SPAN {} still serves them",
        String::from_utf8_lossy(&w.column),
        w.span,
        margin,
        new_span
    ))
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
            let list = cols.iter().map(|c| show(c)).collect::<Vec<_>>().join(" THEN ");
            Some(format!("TABLE.DECLARE {} … ORDERPATH {} ON {}", show(table), show(suffix), list))
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

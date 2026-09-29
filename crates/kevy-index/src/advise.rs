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

#[path = "advise_usage.rs"]
mod usage;
pub use usage::UsageCell;

/// What shape of query was refused — the kind half of the derived
/// declaration.
///
/// ```
/// use kevy_index::{AdviseLog, AdviseShape};
/// let mut log = AdviseLog::new();
/// log.observe(b"t.at", AdviseShape::Range, &[]);
/// // the shape is half the family key: the same name refused as MATCH is a new family
/// log.observe(b"t.at", AdviseShape::Match, &[]);
/// assert_eq!(log.entries().len(), 2);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AdviseShape {
    /// `RANGE`/`EQ` on an undeclared single-column path.
    ///
    /// ```
    /// use kevy_index::{AdviseEntry, AdviseShape};
    /// # let mut cat = kevy_index::TableCatalog::new();
    /// # cat.create(kevy_index::parse_table_declare(&[
    /// #     b"TABLE.DECLARE", b"t", b"PREFIX", b"t:", b"PK", b"id", b"COLUMN", b"id", b"i64",
    /// #     b"COLUMN", b"at", b"i64", b"COLUMN", b"body", b"str", b"INDEX", b"at", b"range",
    /// # ])?)?;
    /// let a = AdviseEntry::new("t.at", AdviseShape::Range, 1).advice(&cat);
    /// assert!(a.is_some_and(|a| a.contains("INDEX at range")));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Range,
    /// `WHERE` naming these columns, in clause order, on a name with
    /// no composite path.
    ///
    /// ```
    /// use kevy_index::{AdviseEntry, AdviseShape};
    /// # let mut cat = kevy_index::TableCatalog::new();
    /// # cat.create(kevy_index::parse_table_declare(&[
    /// #     b"TABLE.DECLARE", b"t", b"PREFIX", b"t:", b"PK", b"id", b"COLUMN", b"id", b"i64",
    /// #     b"COLUMN", b"at", b"i64", b"COLUMN", b"body", b"str", b"INDEX", b"at", b"range",
    /// # ])?)?;
    /// let shape = AdviseShape::Where(vec![b"body".to_vec(), b"at".to_vec()]);
    /// let a = AdviseEntry::new("t.by_body", shape, 1).advice(&cat);
    /// assert!(a.is_some_and(|a| a.contains("ORDERPATH by_body ON body THEN at")));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Where(Vec<Vec<u8>>),
    /// `MATCH` on an undeclared text path.
    ///
    /// ```
    /// use kevy_index::{AdviseEntry, AdviseShape};
    /// # let mut cat = kevy_index::TableCatalog::new();
    /// # cat.create(kevy_index::parse_table_declare(&[
    /// #     b"TABLE.DECLARE", b"t", b"PREFIX", b"t:", b"PK", b"id", b"COLUMN", b"id", b"i64",
    /// #     b"COLUMN", b"at", b"i64", b"COLUMN", b"body", b"str", b"INDEX", b"at", b"range",
    /// # ])?)?;
    /// let a = AdviseEntry::new("t.body", AdviseShape::Match, 1).advice(&cat);
    /// assert!(a.is_some_and(|a| a.contains("FIELD body TYPE str KIND text")));
    /// // only a str column can carry a text index
    /// assert_eq!(AdviseEntry::new("t.at", AdviseShape::Match, 1).advice(&cat), None);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Match,
    /// A clause naming a field the (existing) path does not store: a
    /// `FILTER`, or `FIELDS` on a global index, which answers them from
    /// its stored values.
    ///
    /// ```
    /// use kevy_index::{AdviseEntry, AdviseShape};
    /// # let mut cat = kevy_index::TableCatalog::new();
    /// # cat.create(kevy_index::parse_table_declare(&[
    /// #     b"TABLE.DECLARE", b"t", b"PREFIX", b"t:", b"PK", b"id", b"COLUMN", b"id", b"i64",
    /// #     b"COLUMN", b"at", b"i64", b"COLUMN", b"body", b"str", b"INDEX", b"at", b"range",
    /// # ])?)?;
    /// let a = AdviseEntry::new("t.at", AdviseShape::Filter(b"body".to_vec()), 1).advice(&cat);
    /// assert!(a.is_some_and(|a| a.starts_with("add VALUES body (type str)")));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Filter(Vec<u8>),
}

/// One observed refusal family: a (name, shape) pair, how often it
/// was refused, and the first argv seen (the human-readable sample).
/// `Clone` so a caller holding the log under a lock can snapshot
/// entries out and render them lock-free.
///
/// ```
/// use kevy_index::{AdviseEntry, AdviseShape};
/// let e = AdviseEntry::new("t.at", AdviseShape::Range, 16);
/// assert_eq!((e.count, e.sample.len()), (16, 0));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct AdviseEntry {
    /// The access-path name the query asked for (`<table>.<suffix>`).
    ///
    /// ```
    /// # use kevy_index::{AdviseLog, AdviseShape};
    /// let mut log = AdviseLog::new();
    /// log.observe(b"orders.at", AdviseShape::Range, &[]);
    /// assert_eq!(log.entries()[0].name, b"orders.at");
    /// ```
    pub name: Vec<u8>,
    /// The refused shape.
    ///
    /// ```
    /// # use kevy_index::{AdviseLog, AdviseShape};
    /// let mut log = AdviseLog::new();
    /// log.observe(b"orders.body", AdviseShape::Match, &[]);
    /// assert_eq!(log.entries()[0].shape, AdviseShape::Match);
    /// ```
    pub shape: AdviseShape,
    /// Refusals observed for this family.
    ///
    /// ```
    /// # use kevy_index::{AdviseLog, AdviseShape};
    /// let mut log = AdviseLog::new();
    /// log.observe(b"orders.at", AdviseShape::Range, &[]);
    /// log.observe(b"orders.at", AdviseShape::Range, &[]);
    /// assert_eq!(log.entries()[0].count, 2);
    /// ```
    pub count: u64,
    /// The first refused argv, verbatim.
    ///
    /// ```
    /// # use kevy_index::{AdviseLog, AdviseShape};
    /// let mut log = AdviseLog::new();
    /// let first = [b"RANGE".to_vec(), b"orders.at".to_vec()];
    /// log.observe(b"orders.at", AdviseShape::Range, &first);
    /// log.observe(b"orders.at", AdviseShape::Range, &[b"EQ".to_vec()]);
    /// assert_eq!(log.entries()[0].sample, first, "later argvs only count");
    /// ```
    pub sample: Vec<Vec<u8>>,
}

impl AdviseEntry {
    /// Render one observed family as the declaration command that would
    /// have served it — executable verbatim, or `None` when the catalog
    /// cannot ground it (unknown table / column: the query itself was
    /// malformed, not under-declared).
    #[must_use]
    ///
    /// ```
    /// use kevy_index::{AdviseEntry, AdviseShape, TableCatalog, parse_table_declare};
    /// let mut cat = TableCatalog::new();
    /// cat.create(parse_table_declare(&[
    ///     b"TABLE.DECLARE", b"t", b"PREFIX", b"t:", b"PK", b"id", b"COLUMN", b"id", b"i64",
    ///     b"COLUMN", b"at", b"i64",
    /// ])?)?;
    /// let e = AdviseEntry::new("t.at", AdviseShape::Range, 3);
    /// assert!(e.advice(&cat).is_some_and(|a| a.contains("INDEX at range")));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn advice(&self, cat: &TableCatalog) -> Option<String> {
        advice_of(self, cat)
    }

    /// Family `(name, shape)` refused `count` times, with no sample argv.
    ///
    /// ```
    /// use kevy_index::{AdviseEntry, AdviseShape};
    /// assert_eq!(AdviseEntry::new("t.body", AdviseShape::Match, 1).name, b"t.body");
    /// ```
    pub fn new(name: impl Into<Vec<u8>>, shape: AdviseShape, count: u64) -> AdviseEntry {
        AdviseEntry { name: name.into(), shape, count, sample: Vec::new() }
    }
}

/// The bounded refusal log. Insertion-ordered; when full, the entry
/// with the SMALLEST count makes room (a family that keeps being
/// refused defends its seat).
///
/// ```
/// use kevy_index::{AdviseLog, AdviseShape};
/// let mut log = AdviseLog::with_cap(2);
/// assert_eq!(log.observe(b"t.a", AdviseShape::Range, &[]), 1);
/// assert_eq!(log.observe(b"t.a", AdviseShape::Range, &[]), 2);
/// log.observe(b"t.b", AdviseShape::Range, &[]);
/// // full: the new family evicts the least-refused one, `t.b`
/// log.observe(b"t.c", AdviseShape::Range, &[]);
/// let names: Vec<_> = log.entries().iter().map(|e| e.name.clone()).collect();
/// assert_eq!(names, [b"t.a".to_vec(), b"t.c".to_vec()]);
/// log.clear();
/// assert!(log.entries().is_empty());
/// ```
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
///
/// ```
/// use kevy_index::{ADVISE_CAP, AdviseLog, AdviseShape};
/// let mut log = AdviseLog::new();
/// for i in 0..=ADVISE_CAP {
///     log.observe(format!("t.c{i}").as_bytes(), AdviseShape::Range, &[]);
/// }
/// assert_eq!(log.entries().len(), ADVISE_CAP);
/// ```
pub const ADVISE_CAP: usize = 128;

/// Refusals a family must accumulate before the auto loop declares
/// for it (tables that opted in via `AUTODECLARE <n>`). Empirical
/// knob, not a derivation.
///
/// ```
/// use kevy_index::{AUTODECLARE_AFTER, AdviseLog, AdviseShape};
/// let mut log = AdviseLog::new();
/// let n = (0..AUTODECLARE_AFTER).map(|_| log.observe(b"t.at", AdviseShape::Range, &[])).last();
/// assert_eq!(n, Some(AUTODECLARE_AFTER), "the refusal that crosses the threshold");
/// ```
pub const AUTODECLARE_AFTER: u64 = 16;

pub(crate) fn apply_auto(spec: &mut TableSpec, e: &AdviseEntry) -> Option<Vec<u8>> {
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
            let on = cols.iter().map(|c| (c.clone(), kevy_text::SortOrder::Asc)).collect();
            spec.orderpaths.push(crate::table::OrderPath::new(suffix, on));
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
        self.entries
            .push(AdviseEntry { sample: argv.to_vec(), ..AdviseEntry::new(name, shape, 1) });
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

pub(crate) fn narrow_advice(spec: &TableSpec, margin: Option<i64>) -> Option<String> {
    let w = spec.window.as_ref()?;
    let margin = margin?;
    if w.bucket <= 0 {
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

pub(crate) fn advice_of(e: &AdviseEntry, cat: &TableCatalog) -> Option<String> {
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

impl TableSpec {
    /// Apply one refused family to the table's declaration — the auto
    /// half of the loop, shared by both faces so they cannot derive
    /// different declarations. Returns the ledger entry recorded in
    /// `auto_added` (`path` or `path#field`); `None` when the budget is
    /// spent, the shape is not one a table declaration serves (`MATCH`
    /// stays advise-only — a text index carries knobs the loop must not
    /// pick), the name does not ground, or it is already declared.
    ///
    /// ```
    /// use kevy_index::{AdviseEntry, AdviseShape, parse_table_declare};
    /// let mut t = parse_table_declare(&[
    ///     b"TABLE.DECLARE", b"t", b"PREFIX", b"t:", b"PK", b"id", b"COLUMN", b"id", b"i64",
    ///     b"COLUMN", b"at", b"i64", b"AUTODECLARE", b"1",
    /// ])?;
    /// let e = AdviseEntry::new("t.at", AdviseShape::Range, 16);
    /// assert_eq!(t.apply_auto(&e), Some(b"t.at".to_vec()));
    /// assert_eq!(t.apply_auto(&e), None, "the budget of one is spent");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn apply_auto(&mut self, e: &AdviseEntry) -> Option<Vec<u8>> {
        apply_auto(self, e)
    }

    /// The window-narrowing advice for one windowed table, given the
    /// smallest probe margin any query on one of its paths has shown:
    /// `Some` when every observed query kept more than a bucket of
    /// margin, so SPAN can shrink by the bucket-aligned amount.
    /// Advise-only — the window synthesis point; the engine never
    /// narrows on its own.
    #[must_use]
    ///
    /// ```
    /// use kevy_index::parse_table_declare;
    /// let t = parse_table_declare(&[
    ///     b"TABLE.DECLARE", b"t", b"PREFIX", b"t:", b"PK", b"id", b"COLUMN", b"id", b"i64",
    ///     b"COLUMN", b"at", b"i64", b"INDEX", b"at", b"range",
    ///     b"WINDOW", b"at", b"SPAN", b"100", b"BUCKET", b"10",
    /// ])?;
    /// assert_eq!(t.narrow_advice(None), None, "no query observed yet");
    /// assert!(t.narrow_advice(Some(37)).is_some_and(|a| a.contains("SPAN 70")));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn narrow_advice(&self, margin: Option<i64>) -> Option<String> {
        narrow_advice(self, margin)
    }
}

#[cfg(test)]
#[path = "advise_tests.rs"]
mod tests;

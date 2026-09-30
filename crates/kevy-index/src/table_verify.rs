//! Named verify-report types, shared by both faces.
//!
//! The first release shipped the embedded report as
//! `(Vec<(Vec<u8>, [u64; 6])>, [u64; 2])` — six unnamed counters per
//! index and two more for the spot check. The dogfood report's F10
//! ("two counters side by side with different time semantics and
//! nothing saying so") is the direct consequence: an anonymous array
//! has nowhere to write what a number means. These structs are where
//! that goes.

/// Per-index verification counters, one row of `TABLE.VERIFY`.
///
/// ```
/// let mut v = kevy_index::IndexVerify::new("t.at");
/// v.entries = 3;
/// assert_eq!((v.name.as_slice(), v.entries, v.drift), (&b"t.at"[..], 3, 0));
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct IndexVerify {
    /// Compiled index name (`<table>.<column-or-orderpath>`).
    ///
    /// ```
    /// use kevy_index::{IndexVerify, parse_table_declare};
    /// let t = parse_table_declare(&[
    ///     b"TABLE.DECLARE", b"t", b"PREFIX", b"t:", b"PK", b"id", b"COLUMN", b"id", b"i64",
    ///     b"COLUMN", b"at", b"i64", b"INDEX", b"at", b"range",
    /// ])?;
    /// // a verifier opens one row per compiled index, under the compiled name
    /// let rows: Vec<IndexVerify> = t.compile()?.iter().map(|i| IndexVerify::new(i.name())).collect();
    /// assert_eq!(rows[0].name, b"t.at");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub name: Vec<u8>,
    /// Entries currently held by the index.
    ///
    /// ```
    /// let mut v = kevy_index::IndexVerify::new("t.at");
    /// (v.rows, v.entries, v.absent) = (5, 4, 1);
    /// // every walked row that derives a value is held
    /// assert_eq!(v.rows - v.absent - v.coerce_failures - v.excluded - v.missing, v.entries);
    /// ```
    pub entries: u64,
    /// Approximate resident bytes of the index structure.
    ///
    /// ```
    /// let mut v = kevy_index::IndexVerify::new("t.at");
    /// (v.entries, v.approx_bytes) = (1_000, 48_000);
    /// assert_eq!(v.approx_bytes / v.entries, 48, "resident bytes per entry");
    /// ```
    pub approx_bytes: u64,
    /// Rows whose driving column is **present but fails to coerce** to
    /// the declared type. Recomputed fresh on every call —
    /// the 4.0 counter of this name was a lifetime tally that also
    /// swallowed absent-column rows, which is how a healthy migration
    /// once read as 30 152 live failures.
    ///
    /// ```
    /// use kevy_index::{IndexValue, ValType};
    /// let mut v = kevy_index::IndexVerify::new("t.at");
    /// // the rows hold `at` = "12" and `at` = "soon"; only the second fails to coerce
    /// for raw in [&b"12"[..], b"soon"] {
    ///     if IndexValue::coerce(ValType::I64, raw).is_none() {
    ///         v.coerce_failures += 1;
    ///     }
    /// }
    /// assert_eq!(v.coerce_failures, 1);
    /// ```
    pub coerce_failures: u64,
    /// Rows excluded because a string component exceeded the composite
    /// bound (`MAX_STR_COMPONENT`). Fresh. The counter that turns a
    /// silent two-row entries gap into a named number.
    ///
    /// ```
    /// let mut v = kevy_index::IndexVerify::new("t.by_name");
    /// (v.rows, v.entries, v.excluded) = (10, 8, 2);
    /// // the two-row gap between rows and entries has a name
    /// assert_eq!(v.rows - v.entries, v.excluded);
    /// ```
    pub excluded: u64,
    /// Rows whose driving column is absent — NULL semantics, excluded
    /// by design and *not* a failure. Fresh; named so absence can never
    /// again masquerade as coercion failure.
    ///
    /// ```
    /// let mut v = kevy_index::IndexVerify::new("t.at");
    /// (v.rows, v.entries, v.absent) = (5, 3, 2);
    /// // rows without the column are excluded by design, not failures
    /// assert_eq!((v.rows - v.entries, v.coerce_failures), (v.absent, 0));
    /// ```
    pub absent: u64,
    /// Prefix rows this verify walked for the row→index direction.
    ///
    /// ```
    /// let mut v = kevy_index::IndexVerify::new("t.at");
    /// (v.rows, v.entries) = (4, 4);
    /// assert_eq!(v.rows, v.entries, "every walked row is indexed");
    /// ```
    pub rows: u64,
    /// Rows that derive a value yet have **no entry** in the index —
    /// the "writer forgot this path" class a drift walk structurally
    /// cannot see, because it iterates entries and a missing entry is
    /// not there to iterate. Fresh.
    ///
    /// ```
    /// let mut v = kevy_index::IndexVerify::new("t.at");
    /// (v.rows, v.entries, v.checked, v.missing) = (4, 3, 3, 1);
    /// // an entry that was never written is invisible to `drift`, visible here
    /// assert_eq!((v.drift, v.missing), (0, 1));
    /// ```
    pub missing: u64,
    /// Distinct sort keys held by more than one row. Non-zero means the
    /// sort is not a total order — a paged reader can skip or repeat
    /// rows at page boundaries. Add a bounded tie-break column.
    ///
    /// ```
    /// use kevy_index::{ValType, order_key};
    /// let mut v = kevy_index::IndexVerify::new("t.at");
    /// // two rows share the sort key 7: the order between them is not defined
    /// let keys = [order_key(ValType::I64, b"7"), order_key(ValType::I64, b"7"), order_key(ValType::I64, b"9")];
    /// v.duplicates = (1..keys.len()).filter(|&i| keys[i] == keys[i - 1]).count() as u64;
    /// assert_eq!(v.duplicates, 1);
    /// ```
    pub duplicates: u64,
    /// Entries whose held value disagrees with re-deriving from the row
    /// right now. Recomputed fresh on every call.
    ///
    /// ```
    /// let mut v = kevy_index::IndexVerify::new("t.at");
    /// (v.checked, v.drift) = (100, 0);
    /// assert_eq!(v.drift, 0, "every checked entry matches its row");
    /// ```
    pub drift: u64,
    /// Entries the drift recheck examined this call.
    ///
    /// ```
    /// let mut v = kevy_index::IndexVerify::new("t.at");
    /// (v.entries, v.checked, v.drift) = (100, 100, 2);
    /// assert_eq!(v.drift * 100 / v.checked, 2, "percent of this pass that drifted");
    /// ```
    pub checked: u64,
}

impl IndexVerify {
    /// All-zero counters for compiled index `name`.
    ///
    /// ```
    /// assert_eq!(kevy_index::IndexVerify::new("t.x").checked, 0);
    /// ```
    pub fn new(name: impl Into<Vec<u8>>) -> IndexVerify {
        IndexVerify { name: name.into(), ..IndexVerify::default() }
    }
}

/// The whole `TABLE.VERIFY` answer.
///
/// ```
/// let mut t = kevy_index::TableVerify::default();
/// t.per_index.push(kevy_index::IndexVerify::new("t.at"));
/// assert_eq!((t.per_index.len(), t.spot_rows), (1, 0));
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct TableVerify {
    /// One row per compiled index of the table.
    ///
    /// ```
    /// use kevy_index::{IndexVerify, TableVerify, parse_table_declare};
    /// let t = parse_table_declare(&[
    ///     b"TABLE.DECLARE", b"t", b"PREFIX", b"t:", b"PK", b"id", b"COLUMN", b"id", b"i64",
    ///     b"COLUMN", b"at", b"i64", b"INDEX", b"at", b"range", b"INDEX", b"id", b"unique",
    /// ])?;
    /// let mut report = TableVerify::default();
    /// report.per_index = t.compile()?.iter().map(|i| IndexVerify::new(i.name())).collect();
    /// assert_eq!(report.per_index.len(), 2);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub per_index: Vec<IndexVerify>,
    /// Rows the bounded column-type spot check sampled this call.
    ///
    /// ```
    /// let mut report = kevy_index::TableVerify::default();
    /// (report.spot_rows, report.spot_type_mismatches) = (64, 0);
    /// assert_eq!(report.spot_type_mismatches, 0, "all 64 sampled rows match their types");
    /// ```
    pub spot_rows: u64,
    /// Of those, rows holding a value that contradicts the declared
    /// column type.
    ///
    /// ```
    /// use kevy_index::{IndexValue, ValType};
    /// let mut report = kevy_index::TableVerify::default();
    /// // a sampled row holds "abc" in a column declared i64
    /// for raw in [&b"1"[..], b"abc"] {
    ///     report.spot_rows += 1;
    ///     if IndexValue::coerce(ValType::I64, raw).is_none() {
    ///         report.spot_type_mismatches += 1;
    ///     }
    /// }
    /// assert_eq!((report.spot_rows, report.spot_type_mismatches), (2, 1));
    /// ```
    pub spot_type_mismatches: u64,
}

/// What `table_ensure` found (the boot verb — see the embedded docs).
///
/// The decision the boot verb makes, in terms of this crate:
///
/// ```
/// use kevy_index::{TableCatalog, TableEnsure, TableSpec, parse_table_declare, spec_diff};
///
/// fn ensure(cat: &mut TableCatalog, spec: TableSpec) -> Result<TableEnsure, String> {
///     match cat.get(&spec.name) {
///         None => {
///             cat.create(spec).map_err(|e| e.to_string())?;
///             Ok(TableEnsure::Created)
///         }
///         Some(cur) if cur.sans_auto() == spec => Ok(TableEnsure::Unchanged),
///         Some(cur) => Err(spec_diff(cur, &spec)),
///     }
/// }
///
/// let decl = |pk: &'static [u8]| {
///     parse_table_declare(&[b"TABLE.DECLARE", b"t", b"PREFIX", b"t:", b"PK", pk, b"COLUMN", pk, b"i64"])
/// };
/// let mut cat = TableCatalog::new();
/// assert_eq!(ensure(&mut cat, decl(b"id")?)?, TableEnsure::Created);
/// assert_eq!(ensure(&mut cat, decl(b"id")?)?, TableEnsure::Unchanged);
/// assert!(ensure(&mut cat, decl(b"uid")?).is_err());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TableEnsure {
    /// The table did not exist; it was declared and its indexes built.
    ///
    /// ```
    /// use kevy_index::TableEnsure;
    /// // the wire reply a boot path answers with
    /// let reply = |e: TableEnsure| match e {
    ///     TableEnsure::Unchanged => "+UNCHANGED",
    ///     _ => "+OK",
    /// };
    /// assert_eq!(reply(TableEnsure::Created), "+OK");
    /// ```
    Created,
    /// An identical declaration already exists; nothing was touched.
    ///
    /// ```
    /// use kevy_index::TableEnsure;
    /// let first_boot = TableEnsure::Created;
    /// let every_later_boot = TableEnsure::Unchanged;
    /// assert_ne!(first_boot, every_later_boot, "only the first boot builds anything");
    /// ```
    Unchanged,
}

/// Name the first difference between an admitted spec and a proposed
/// one — the message `table_ensure` refuses with. Specific enough to
/// act on, short enough for a boot log.
///
/// ```
/// use kevy_index::{parse_table_declare, spec_diff};
/// let cur = parse_table_declare(&[
///     b"TABLE.DECLARE", b"t", b"PREFIX", b"t:", b"PK", b"id", b"COLUMN", b"id", b"i64",
/// ])?;
/// let new = parse_table_declare(&[
///     b"TABLE.DECLARE", b"t", b"PREFIX", b"t2:", b"PK", b"id", b"COLUMN", b"id", b"i64",
/// ])?;
/// let msg = spec_diff(&cur, &new);
/// assert!(msg.starts_with("ERR table 't' exists with a different spec (PREFIX differ)"));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[must_use]
pub fn spec_diff(cur: &crate::TableSpec, new: &crate::TableSpec) -> String {
    let part = if cur.prefix != new.prefix {
        "PREFIX"
    } else if cur.pk != new.pk {
        "PK"
    } else if cur.columns != new.columns {
        "COLUMNS"
    } else if cur.indexes != new.indexes {
        "INDEXES"
    } else if cur.orderpaths != new.orderpaths {
        "ORDERPATHS"
    } else {
        "SPEC"
    };
    format!(
        "ERR table '{}' exists with a different spec ({part} differ); \
         TABLE.REPLACE rebuilds, TABLE.DROP + DECLARE is the manual form",
        String::from_utf8_lossy(&cur.name)
    )
}

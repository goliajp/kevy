//! Composite (multi-column) Range indexes — the ORDERPATH engine
//! piece.
//!
//! A composite index derives ONE order-preserving byte string per row
//! from several declared columns, so a single `(value, key)` B-tree
//! answers "WHERE a = x ORDER BY b DESC" the way a relational composite
//! B-tree does. The derivation is a pure mechanical byte encoding of
//! declared fields — no semantics, no planning; `IDX.VERIFY` recomputes
//! it, so drift stays falsifiable (derived-by-construction).
//!
//! ## Encoding (the exact byte rules)
//!
//! Per component, in declared order:
//! * `i64` — [`order_key`]'s sign-flipped big-endian, fixed 8 bytes.
//! * `f64` — [`order_key`]'s IEEE total-order transform, fixed 8 bytes.
//! * `str` — the raw bytes with `0x00` escaped as `0x00 0xFF`, then the
//!   terminator `0x00 0x00`. The terminator sorts below every escaped
//!   continuation byte, so a prefix string sorts first and the
//!   concatenation stays unambiguous (self-delimiting).
//! * A `DESC` component complements every byte of its framed encoding —
//!   order-reversing, and still self-delimiting because complementing
//!   is a bijection on the frame.
//!
//! Components concatenate; `memcmp` of two encodings equals the
//! column-wise tuple comparison (DESC columns reversed). A row missing
//! a component column (or one that fails coercion, or a `str` component
//! longer than [`MAX_STR_COMPONENT`]) is EXCLUDED from the composite
//! index — the same exclusion semantics a scalar coerce failure has.

use crate::catalog::{IndexSpec, ValType};
use crate::value::{IndexValue, order_key};

/// One declared composite column: which hash field, how its bytes
/// coerce/order, and whether this component sorts descending.
///
/// The type is carried per column (not looked up at read time) so the
/// sidecar reload reproduces the exact same byte derivation — an
/// encoding the catalog cannot reconstruct is index drift at boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositeCol {
    /// Hash field name.
    pub name: Vec<u8>,
    /// How the column's bytes coerce (i64 | f64 | str).
    pub ty: ValType,
    /// Descending component (bytes complemented).
    pub desc: bool,
}

/// Hard cap on composite columns per index.
pub const MAX_COMPOSITE_COLS: usize = 8;

/// Hard cap on one `str` component's raw length. A longer value
/// excludes the row (documented, conformance-tested) — the same class
/// of limit a relational B-tree puts on its index row size, and what
/// keeps [`composite_bounds`]' upper bound finite and exact.
pub const MAX_STR_COMPONENT: usize = 255;

/// The named refusal for `WHERE` on an index that declares no
/// composite columns. Shared verbatim by the server and the embedded
/// dispatch so the wire wording cannot drift.
pub const WHERE_NOT_COMPOSITE: &str =
    "WHERE requires a composite index (an ORDERPATH-compiled one) — this index is not one";

/// Encode one component. `None` = the row is excluded.
fn encode_component(col: &CompositeCol, raw: &[u8]) -> Option<Vec<u8>> {
    let mut framed = match col.ty {
        ValType::I64 | ValType::F64 => order_key(col.ty, raw)?,
        ValType::Str => {
            if raw.len() > MAX_STR_COMPONENT {
                return None;
            }
            let mut out = Vec::with_capacity(raw.len() + 2);
            for &b in raw {
                out.push(b);
                if b == 0x00 {
                    out.push(0xFF);
                }
            }
            out.extend_from_slice(&[0x00, 0x00]);
            out
        }
        ValType::Vector => return None,
    };
    if col.desc {
        for b in &mut framed {
            *b = !*b;
        }
    }
    Some(framed)
}

/// The row's composite encoding: order-preserving concatenation of the
/// declared columns. `None` = the row is excluded (a missing column, a
/// coerce failure, or an over-long `str` component).
pub fn composite_encode(cols: &[CompositeCol], vals: &[Option<&[u8]>]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for (col, raw) in cols.iter().zip(vals) {
        out.extend_from_slice(&encode_component(col, (*raw)?)?);
    }
    Some(out)
}

/// One parsed `WHERE` clause: an equality prefix plus an optional range
/// on the next component. Grammar lives here so the server and the
/// embedded dispatch parse the identical shape.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WhereClause {
    /// `col EQ v` pairs, wire order.
    pub eqs: Vec<(Vec<u8>, Vec<u8>)>,
    /// `RANGE col min max`, at most one, after the equalities.
    pub range: Option<(Vec<u8>, Vec<u8>, Vec<u8>)>,
}

/// Parse `WHERE <col> EQ <v> [<col> EQ <v>…] [RANGE <col> <min> <max>]`
/// starting at `at` (the token after `WHERE`). `stop` names the clause
/// keywords that end the WHERE block (LIMIT / FILTER / …). Returns the
/// clause plus the index of the first unconsumed token. `None` = syntax
/// error (empty WHERE included — accepting one that constrains nothing
/// would be the accept-and-ignore shape).
pub fn parse_where(
    argv: &[Vec<u8>],
    at: usize,
    stop: impl Fn(&[u8]) -> bool,
) -> Option<(WhereClause, usize)> {
    let mut w = WhereClause::default();
    let mut i = at;
    while i < argv.len() && !stop(&argv[i]) {
        if argv[i].eq_ignore_ascii_case(b"RANGE") {
            let col = argv.get(i + 1)?.clone();
            let min = argv.get(i + 2)?.clone();
            let max = argv.get(i + 3)?.clone();
            w.range = Some((col, min, max));
            i += 4;
            // RANGE is terminal within WHERE: composite-btree semantics
            // stop at the first ranged component.
            break;
        }
        if !argv.get(i + 1)?.eq_ignore_ascii_case(b"EQ") {
            return None;
        }
        w.eqs.push((argv[i].clone(), argv.get(i + 2)?.clone()));
        i += 3;
    }
    if w.eqs.is_empty() && w.range.is_none() {
        return None;
    }
    Some((w, i))
}

fn declared_list(cols: &[CompositeCol]) -> String {
    cols.iter()
        .map(|c| String::from_utf8_lossy(&c.name).into_owned())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Encode one WHERE bound value for `col`, or the named error.
fn bound_component(col: &CompositeCol, raw: &[u8]) -> Result<Vec<u8>, String> {
    encode_component(col, raw).ok_or_else(|| {
        format!(
            "WHERE bound '{}' is not a valid {}, which is how this composite declares '{}'",
            String::from_utf8_lossy(raw),
            col.ty.tag(),
            String::from_utf8_lossy(&col.name),
        )
    })
}

/// The memcmp-maximum encoding one component can produce (numeric =
/// eight `0xFF`; DESC str = the empty string's complemented frame; ASC
/// str = unbounded, answered with a dominating pad — see
/// [`composite_bounds`]).
fn component_max(col: &CompositeCol) -> Vec<u8> {
    match (col.ty, col.desc) {
        (ValType::I64 | ValType::F64, _) => vec![0xFF; 8],
        (ValType::Str, true) => vec![0xFF, 0xFF],
        // An ASC str encoding is at most 2×MAX_STR_COMPONENT escaped
        // bytes + the 2-byte terminator, and always carries a 0x00, so
        // a solid 0xFF run one byte longer strictly dominates every
        // valid encoding. Nothing valid can equal it (no terminator),
        // so the inclusive upper bound stays exact.
        (ValType::Str, false) => vec![0xFF; MAX_STR_COMPONENT * 2 + 3],
        (ValType::Vector, _) => Vec::new(),
    }
}

/// Turn "WHERE a = x [AND b range]" into the byte-range over the
/// encoded tuple — classic composite-btree semantics: the equality
/// prefix pins leading components, the optional range constrains the
/// next one, everything after is unconstrained. The WHERE columns must
/// be a leading prefix of the composite's declared order — anything
/// else is a named error, never a scan.
///
/// Both bounds are INCLUSIVE and exact over valid encodings (the
/// segment only ever holds derived encodings).
pub fn composite_bounds(
    cols: &[CompositeCol],
    w: &WhereClause,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut lo = Vec::new();
    let mut hi = Vec::new();
    let mut at = 0usize;
    for (name, value) in &w.eqs {
        let col = resolve_col(cols, at, name)?;
        let enc = bound_component(col, value)?;
        lo.extend_from_slice(&enc);
        hi.extend_from_slice(&enc);
        at += 1;
    }
    if let Some((name, min, max)) = &w.range {
        let col = resolve_col(cols, at, name)?;
        let a = bound_component(col, min)?;
        let b = bound_component(col, max)?;
        // A DESC component reverses the encoded order, so the encoded
        // interval endpoints swap; byte-wise min/max keeps both
        // directions on one path.
        let (emin, emax) = if a <= b { (a, b) } else { (b, a) };
        lo.extend_from_slice(&emin);
        hi.extend_from_slice(&emax);
        at += 1;
    }
    // Unconstrained tail components: the lower bound extends by
    // nothing (any continuation only grows the string); the upper
    // bound extends by each component's maximum until one dominates
    // strictly (the ASC-str pad), after which further bytes are moot.
    for col in &cols[at..] {
        let m = component_max(col);
        let dominates = col.ty == ValType::Str && !col.desc;
        hi.extend_from_slice(&m);
        if dominates {
            break;
        }
    }
    Ok((lo, hi))
}

/// The WHERE column at position `at` — which MUST be the composite's
/// `at`-th declared column (prefix rule), and declared at all.
fn resolve_col<'c>(
    cols: &'c [CompositeCol],
    at: usize,
    name: &[u8],
) -> Result<&'c CompositeCol, String> {
    if !cols.iter().any(|c| c.name == name) {
        return Err(format!(
            "WHERE names column '{}', which this composite does not declare — it declares: {}",
            String::from_utf8_lossy(name),
            declared_list(cols),
        ));
    }
    match cols.get(at) {
        Some(c) if c.name == name => Ok(c),
        _ => Err(format!(
            "WHERE columns must be a leading prefix of the composite's declared order ({})",
            declared_list(cols),
        )),
    }
}

/// The CREATE-time guard: composite is legal ONLY on `KIND range` with
/// `TYPE str` (the derived value IS a byte string), a single declared
/// FIELD, no stored VALUES, and 1..=[`MAX_COMPOSITE_COLS`] columns of
/// scalar types. Every refused combo errors by name.
pub(crate) fn composite_guard(spec: &IndexSpec) -> Result<(), &'static str> {
    let Some(cols) = &spec.composite else { return Ok(()) };
    if spec.kind != crate::IndexKind::Range {
        return Err("ERR COMPOSITE requires KIND range");
    }
    if spec.ty != ValType::Str {
        return Err("ERR COMPOSITE requires TYPE str");
    }
    if !spec.values.is_empty() {
        return Err("ERR COMPOSITE cannot combine with VALUES");
    }
    if spec.fields.len() != 1 {
        return Err("ERR COMPOSITE declares exactly one FIELD");
    }
    if cols.is_empty() {
        return Err("ERR COMPOSITE needs at least one column");
    }
    if cols.len() > MAX_COMPOSITE_COLS {
        return Err("ERR COMPOSITE supports at most 8 columns");
    }
    if cols.iter().any(|c| matches!(c.ty, ValType::Vector)) {
        return Err("ERR COMPOSITE columns must be i64|f64|str");
    }
    Ok(())
}

impl IndexSpec {
    /// Column names a scalar (range/unique) row read fetches, in
    /// order: the driving columns — the composite's declared columns,
    /// or the single `FIELD` — then the declared `VALUES` columns.
    /// One row peek covers everything (the one-pread-per-row rule).
    pub fn scalar_read_names(&self) -> Vec<&[u8]> {
        let mut names: Vec<&[u8]> = match &self.composite {
            Some(cols) => cols.iter().map(|c| c.name.as_slice()).collect(),
            None => vec![self.field()],
        };
        names.extend(self.values.iter().map(|v| v.name.as_slice()));
        names
    }

    /// How many leading [`Self::scalar_read_names`] drive the index
    /// value (the rest are stored `VALUES`).
    pub fn primary_width(&self) -> usize {
        self.composite.as_ref().map_or(1, Vec::len)
    }

    /// Derive the index value from the fetched driving columns
    /// (parallel to the first [`Self::primary_width`] names). `None` =
    /// the row is excluded. This is THE single derivation both the
    /// server and the embedded store apply — and what `IDX.VERIFY`
    /// recomputes, so composite drift is falsifiable too.
    pub fn derive_scalar(&self, prim: &[Option<Vec<u8>>]) -> Option<IndexValue> {
        match &self.composite {
            Some(cols) => {
                let refs: Vec<Option<&[u8]>> = prim.iter().map(|o| o.as_deref()).collect();
                composite_encode(cols, &refs).map(IndexValue::Str)
            }
            None => IndexValue::coerce(self.ty, prim.first()?.as_deref()?),
        }
    }
}

#[cfg(test)]
#[path = "composite_tests.rs"]
mod tests;

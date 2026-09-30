//! `WHERE` on a composite index: the clause grammar and the byte range
//! it selects over the encoded tuple.

use super::{CompositeCol, MAX_STR_COMPONENT, encode_component};
use crate::catalog::ValType;
use crate::table_error::WhereError;
use kevy_text::SortOrder;

/// One parsed `WHERE` clause: an equality prefix plus an optional range
/// on the next component. Grammar lives here so the server and the
/// embedded dispatch parse the identical shape.
///
/// ```
/// use kevy_index::{WhereClause, parse_where};
/// let argv: Vec<Vec<u8>> = ["WHERE", "region", "EQ", "eu"].iter().map(|s| s.as_bytes().to_vec()).collect();
/// let (w, _) = parse_where(&argv, 1, |_| false).expect("valid");
/// let mut want = WhereClause::default();
/// want.eqs.push((b"region".to_vec(), b"eu".to_vec()));
/// assert_eq!(w, want);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct WhereClause {
    /// `col EQ v` pairs, wire order.
    ///
    /// ```
    /// # let argv: Vec<Vec<u8>> = ["WHERE", "region", "EQ", "eu", "RANGE", "at", "10", "20", "LIMIT", "5"]
    /// #     .iter().map(|s| s.as_bytes().to_vec()).collect();
    /// let (w, _) = kevy_index::parse_where(&argv, 1, |t| t.eq_ignore_ascii_case(b"LIMIT")).expect("valid");
    /// assert_eq!(w.eqs, [(b"region".to_vec(), b"eu".to_vec())]);
    /// ```
    pub eqs: Vec<(Vec<u8>, Vec<u8>)>,
    /// `RANGE col min max`, at most one, after the equalities.
    ///
    /// ```
    /// # let argv: Vec<Vec<u8>> = ["WHERE", "region", "EQ", "eu", "RANGE", "at", "10", "20", "LIMIT", "5"]
    /// #     .iter().map(|s| s.as_bytes().to_vec()).collect();
    /// let (w, _) = kevy_index::parse_where(&argv, 1, |t| t.eq_ignore_ascii_case(b"LIMIT")).expect("valid");
    /// assert_eq!(w.range, Some((b"at".to_vec(), b"10".to_vec(), b"20".to_vec())));
    /// ```
    pub range: Option<(Vec<u8>, Vec<u8>, Vec<u8>)>,
}

/// Parse `WHERE <col> EQ <v> [<col> EQ <v>…] [RANGE <col> <min> <max>]`
/// starting at `at` (the token after `WHERE`). `stop` names the clause
/// keywords that end the WHERE block (LIMIT / FILTER / …). Returns the
/// clause plus the index of the first unconsumed token. `None` = syntax
/// error (empty WHERE included — accepting one that constrains nothing
/// would be the accept-and-ignore shape).
///
/// ```
/// use kevy_index::parse_where;
/// let argv: Vec<Vec<u8>> = ["IDX.QUERY", "t.p", "WHERE", "a", "EQ", "1", "b", "EQ", "2", "LIMIT", "5"]
///     .iter().map(|s| s.as_bytes().to_vec()).collect();
/// let (w, next) = parse_where(&argv, 3, |t| t.eq_ignore_ascii_case(b"LIMIT")).expect("valid");
/// assert_eq!(w.eqs.len(), 2);
/// assert_eq!(argv[next], b"LIMIT", "parsing stops at the first stop keyword");
/// // an empty WHERE and a pair without EQ are syntax errors
/// assert!(parse_where(&argv, 9, |t| t == b"LIMIT").is_none());
/// assert!(parse_where(&argv, 4, |_| false).is_none());
/// ```
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

fn declared_list(cols: &[CompositeCol]) -> Vec<Vec<u8>> {
    cols.iter().map(|c| c.name.clone()).collect()
}

/// Encode one WHERE bound value for `col`, or the named error.
fn bound_component(col: &CompositeCol, raw: &[u8], now: i64) -> Result<Vec<u8>, WhereError> {
    // Resolve `@` time expressions BEFORE the shared encoder: row
    // derivation (`classify_component`) shares `encode_component` and
    // must never interpret data — a row whose i64 field holds "@now"
    // is a coerce failure, not an expression. Only a QUERY bound
    // comes through here.
    let resolved;
    let raw = if col.ty == ValType::I64 && raw.first() == Some(&b'@') {
        match kevy_time::eval(raw, now) {
            Some(i) => {
                resolved = i.to_string().into_bytes();
                resolved.as_slice()
            }
            None => {
                return Err(WhereError::TimeExpression {
                    bound: raw.to_vec(),
                    column: col.name.clone(),
                });
            }
        }
    } else {
        raw
    };
    encode_component(col, raw).ok_or_else(|| WhereError::Value {
        bound: raw.to_vec(),
        ty: col.ty,
        column: col.name.clone(),
    })
}

/// The memcmp-maximum encoding one component can produce (numeric =
/// eight `0xFF`; DESC str = the empty string's complemented frame; ASC
/// str = unbounded, answered with a dominating pad — see
/// [`composite_bounds`]).
fn component_max(col: &CompositeCol) -> Vec<u8> {
    match (col.ty, col.order) {
        (ValType::I64 | ValType::F64, _) => vec![0xFF; 8],
        (ValType::Str, SortOrder::Desc) => vec![0xFF, 0xFF],
        // An ASC str encoding is at most 2×MAX_STR_COMPONENT escaped
        // bytes + the 2-byte terminator, and always carries a 0x00, so
        // a solid 0xFF run one byte longer strictly dominates every
        // valid encoding. Nothing valid can equal it (no terminator),
        // so the inclusive upper bound stays exact.
        (ValType::Str, SortOrder::Asc) => vec![0xFF; MAX_STR_COMPONENT * 2 + 3],
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
///
/// ```
/// use kevy_index::{CompositeCol, ValType, WhereError, composite_bounds, composite_encode, parse_where};
/// let cols = [CompositeCol::new("region", ValType::Str), CompositeCol::new("at", ValType::I64)];
/// let argv: Vec<Vec<u8>> = ["region", "EQ", "eu", "RANGE", "at", "10", "20"]
///     .iter().map(|s| s.as_bytes().to_vec()).collect();
/// let (w, _) = parse_where(&argv, 0, |_| false).expect("valid");
/// let (lo, hi) = composite_bounds(&cols, &w, 0)?;
/// let row = |r: &[u8], at: &[u8]| composite_encode(&cols, &[Some(r), Some(at)]).expect("indexed");
/// assert!((lo.clone()..=hi.clone()).contains(&row(b"eu", b"15")));
/// assert!(!(lo..=hi).contains(&row(b"eu", b"21")));
///
/// // the WHERE columns must be a leading prefix of the declared order
/// let (skip, _) = parse_where(&argv[3..], 0, |_| false).expect("valid");
/// assert!(matches!(composite_bounds(&cols, &skip, 0), Err(WhereError::NotLeadingPrefix { .. })));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn composite_bounds(
    cols: &[CompositeCol],
    w: &WhereClause,
    now: i64,
) -> Result<(Vec<u8>, Vec<u8>), WhereError> {
    let mut lo = Vec::new();
    let mut hi = Vec::new();
    let mut at = 0usize;
    for (name, value) in &w.eqs {
        let col = resolve_col(cols, at, name)?;
        let enc = bound_component(col, value, now)?;
        lo.extend_from_slice(&enc);
        hi.extend_from_slice(&enc);
        at += 1;
    }
    if let Some((name, min, max)) = &w.range {
        let col = resolve_col(cols, at, name)?;
        let a = bound_component(col, min, now)?;
        let b = bound_component(col, max, now)?;
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
        let dominates = col.ty == ValType::Str && col.order == SortOrder::Asc;
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
) -> Result<&'c CompositeCol, WhereError> {
    if !cols.iter().any(|c| c.name == name) {
        return Err(WhereError::UnknownColumn {
            column: name.to_vec(),
            declared: declared_list(cols),
        });
    }
    match cols.get(at) {
        Some(c) if c.name == name => Ok(c),
        _ => Err(WhereError::NotLeadingPrefix { declared: declared_list(cols) }),
    }
}

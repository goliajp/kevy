//! The optional clause tail every `IDX.QUERY` shape shares — `LIMIT`,
//! `CURSOR`, `FIELDS`, and the MATCH-only clauses. Split from
//! `idx_query.rs` for the 500-LOC house rule (a `#[path]` child, so the
//! dispatch keeps reaching it directly).

/// One parsed clause tail. `highlight` is `None` unless the query asked
/// for it; `Some(empty)` means every field.
pub(super) struct Tail {
    pub(super) limit: usize,
    pub(super) cursor_raw: Option<Vec<u8>>,
    pub(super) fields: Vec<Vec<u8>>,
    pub(super) highlight: Option<Vec<Vec<u8>>>,
    /// `TYPO n`: edit budget for each bare term; 0 = exact.
    pub(super) typo: u32,
    /// `OFFSET n`: hits to skip before `LIMIT`.
    pub(super) offset: usize,
    /// `IN <field…>`: declared field names to score within; empty = the
    /// whole document.
    pub(super) scope: Vec<Vec<u8>>,
}

/// A tail clause keyword — the boundary a variadic clause collects up to.
fn is_tail_keyword(a: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b"LIMIT")
        || a.eq_ignore_ascii_case(b"CURSOR")
        || a.eq_ignore_ascii_case(b"FIELDS")
        || a.eq_ignore_ascii_case(b"HIGHLIGHT")
        || a.eq_ignore_ascii_case(b"TYPO")
        || a.eq_ignore_ascii_case(b"OFFSET")
        || a.eq_ignore_ascii_case(b"IN")
}

/// Collect a variadic clause's args from `start` until the next keyword.
fn collect_until_keyword(argv: &[Vec<u8>], start: usize) -> (Vec<Vec<u8>>, usize) {
    let mut i = start;
    let mut out = Vec::new();
    while i < argv.len() && !is_tail_keyword(&argv[i]) {
        out.push(argv[i].clone());
        i += 1;
    }
    (out, i)
}

pub(super) fn parse_tail(
    argv: &[Vec<u8>],
    mut i: usize,
    default_limit: usize,
    cap: usize,
    match_clauses: bool,
) -> Option<Tail> {
    let mut t = Tail {
        limit: default_limit,
        cursor_raw: None,
        fields: Vec::new(),
        highlight: None,
        typo: 0,
        offset: 0,
        scope: Vec::new(),
    };
    while i < argv.len() {
        i = apply_tail_clause(argv, i, &mut t, match_clauses)?;
    }
    t.limit = t.limit.clamp(1, cap);
    Some(t)
}

/// Apply the tail clause starting at `i`; returns the next index, or
/// `None` on a syntax error. `match_clauses` gates the MATCH-only ones
/// (HIGHLIGHT / TYPO / OFFSET) so a RANGE query cannot smuggle them in.
fn apply_tail_clause(
    argv: &[Vec<u8>],
    i: usize,
    t: &mut Tail,
    match_clauses: bool,
) -> Option<usize> {
    let a = &argv[i];
    if a.eq_ignore_ascii_case(b"LIMIT") {
        t.limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
        Some(i + 2)
    } else if a.eq_ignore_ascii_case(b"CURSOR") {
        t.cursor_raw = Some(argv.get(i + 1)?.clone());
        Some(i + 2)
    } else if a.eq_ignore_ascii_case(b"FIELDS") {
        let (fs, next) = collect_until_keyword(argv, i + 1);
        if fs.is_empty() {
            return None;
        }
        t.fields = fs;
        Some(next)
    } else if match_clauses && a.eq_ignore_ascii_case(b"HIGHLIGHT") {
        let (hs, next) = collect_until_keyword(argv, i + 1);
        t.highlight = Some(hs);
        Some(next)
    } else if match_clauses && a.eq_ignore_ascii_case(b"TYPO") {
        t.typo = match argv.get(i + 1)?.as_slice() {
            b"0" => 0,
            b"1" => 1,
            b"2" => 2,
            _ => return None,
        };
        Some(i + 2)
    } else if match_clauses && a.eq_ignore_ascii_case(b"OFFSET") {
        t.offset = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
        Some(i + 2)
    } else if match_clauses && a.eq_ignore_ascii_case(b"IN") {
        let (fs, next) = collect_until_keyword(argv, i + 1);
        if fs.is_empty() {
            return None;
        }
        t.scope = fs;
        Some(next)
    } else {
        None
    }
}


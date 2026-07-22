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
    /// `FILTER …`: non-scoring predicates over stored values, ANDed.
    /// Owned here because the borrowed form the store takes points at
    /// them. Only with the text surface: without it there is no MATCH
    /// for a predicate to restrict, so the clause is not a thing rather
    /// than a thing that is ignored.
    #[cfg(feature = "text")]
    pub(super) filters: Vec<FilterClause>,
    /// `SORT <field> ASC|DESC`: select by a stored value rather than by
    /// score.
    #[cfg(feature = "text")]
    pub(super) sort: Option<(Vec<u8>, bool)>,
    /// `DISTINCT <field>`: at most one hit per value of a stored field.
    #[cfg(feature = "text")]
    pub(super) distinct: Option<Vec<u8>>,
    /// `FACET <field…>`: count each field's values over the match set.
    #[cfg(feature = "text")]
    pub(super) facets: Vec<Vec<u8>>,
}

/// One parsed `FILTER <field> RANGE <min> <max>` / `EQ <v>`.
#[cfg(feature = "text")]
pub(super) struct FilterClause {
    pub(super) field: Vec<u8>,
    pub(super) shape: FilterShape,
}

#[cfg(feature = "text")]
pub(super) enum FilterShape {
    Range(Vec<u8>, Vec<u8>),
    Eq(Vec<u8>),
}

#[cfg(feature = "text")]
impl FilterClause {
    /// The borrowed form [`crate::MatchOpts`] takes.
    pub(super) fn as_value_filter(&self) -> crate::ValueFilter<'_> {
        match &self.shape {
            FilterShape::Range(min, max) => {
                crate::ValueFilter::Range { field: &self.field, min, max }
            }
            FilterShape::Eq(v) => crate::ValueFilter::Eq { field: &self.field, value: v },
        }
    }
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
        || a.eq_ignore_ascii_case(b"FILTER")
        || a.eq_ignore_ascii_case(b"SORT")
        || a.eq_ignore_ascii_case(b"DISTINCT")
        || a.eq_ignore_ascii_case(b"FACET")
}

/// One `FILTER <field> RANGE <min> <max>` / `EQ <v>`; several AND, which
/// is why each appends rather than replaces.
/// The clauses only a MATCH takes. Split out so the shared tail
/// dispatcher stays readable as the surface grows.
fn apply_match_clause(argv: &[Vec<u8>], i: usize, t: &mut Tail) -> Option<usize> {
    let a = &argv[i];
    if a.eq_ignore_ascii_case(b"HIGHLIGHT") {
        let (hs, next) = collect_until_keyword(argv, i + 1);
        t.highlight = Some(hs);
        Some(next)
    } else if a.eq_ignore_ascii_case(b"TYPO") {
        t.typo = match argv.get(i + 1)?.as_slice() {
            b"0" => 0,
            b"1" => 1,
            b"2" => 2,
            _ => return None,
        };
        Some(i + 2)
    } else if a.eq_ignore_ascii_case(b"OFFSET") {
        t.offset = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
        Some(i + 2)
    } else if a.eq_ignore_ascii_case(b"FACET") {
        apply_facet(argv, i, t)
    } else if a.eq_ignore_ascii_case(b"DISTINCT") {
        apply_distinct(argv, i, t)
    } else if a.eq_ignore_ascii_case(b"SORT") {
        apply_sort(argv, i, t)
    } else if a.eq_ignore_ascii_case(b"FILTER") {
        apply_filter(argv, i, t)
    } else if a.eq_ignore_ascii_case(b"IN") {
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

/// `SORT <field> ASC|DESC`.
#[cfg(feature = "text")]
fn apply_sort(argv: &[Vec<u8>], i: usize, t: &mut Tail) -> Option<usize> {
    let field = argv.get(i + 1)?.clone();
    let dir = argv.get(i + 2)?;
    let desc = if dir.eq_ignore_ascii_case(b"DESC") {
        true
    } else if dir.eq_ignore_ascii_case(b"ASC") {
        false
    } else {
        return None;
    };
    t.sort = Some((field, desc));
    Some(i + 3)
}

/// `FACET <field…>`.
#[cfg(feature = "text")]
fn apply_facet(argv: &[Vec<u8>], i: usize, t: &mut Tail) -> Option<usize> {
    let (fs, next) = collect_until_keyword(argv, i + 1);
    if fs.is_empty() {
        return None;
    }
    t.facets = fs;
    Some(next)
}

/// No MATCH without the text surface, so nothing to count.
#[cfg(not(feature = "text"))]
fn apply_facet(_argv: &[Vec<u8>], _i: usize, _t: &mut Tail) -> Option<usize> {
    None
}

/// `DISTINCT <field>`.
#[cfg(feature = "text")]
fn apply_distinct(argv: &[Vec<u8>], i: usize, t: &mut Tail) -> Option<usize> {
    t.distinct = Some(argv.get(i + 1)?.clone());
    Some(i + 2)
}

/// No MATCH without the text surface, so nothing to collapse.
#[cfg(not(feature = "text"))]
fn apply_distinct(_argv: &[Vec<u8>], _i: usize, _t: &mut Tail) -> Option<usize> {
    None
}

/// Without the text surface there is no MATCH to order, so `SORT` is a
/// syntax error rather than a clause that parses and does nothing.
#[cfg(not(feature = "text"))]
fn apply_sort(_argv: &[Vec<u8>], _i: usize, _t: &mut Tail) -> Option<usize> {
    None
}

/// Without the text surface there is no MATCH for a predicate to
/// restrict, so `FILTER` is a syntax error rather than a clause that
/// parses and does nothing.
#[cfg(not(feature = "text"))]
fn apply_filter(_argv: &[Vec<u8>], _i: usize, _t: &mut Tail) -> Option<usize> {
    None
}

#[cfg(feature = "text")]
fn apply_filter(argv: &[Vec<u8>], i: usize, t: &mut Tail) -> Option<usize> {
    let field = argv.get(i + 1)?.clone();
    let mode = argv.get(i + 2)?;
    if mode.eq_ignore_ascii_case(b"RANGE") {
        let shape = FilterShape::Range(argv.get(i + 3)?.clone(), argv.get(i + 4)?.clone());
        t.filters.push(FilterClause { field, shape });
        Some(i + 5)
    } else if mode.eq_ignore_ascii_case(b"EQ") {
        t.filters.push(FilterClause { field, shape: FilterShape::Eq(argv.get(i + 3)?.clone()) });
        Some(i + 4)
    } else {
        None
    }
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
        #[cfg(feature = "text")]
        filters: Vec::new(),
        #[cfg(feature = "text")]
        sort: None,
        #[cfg(feature = "text")]
        distinct: None,
        #[cfg(feature = "text")]
        facets: Vec::new(),
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
    } else if match_clauses {
        apply_match_clause(argv, i, t)
    } else {
        None
    }
}


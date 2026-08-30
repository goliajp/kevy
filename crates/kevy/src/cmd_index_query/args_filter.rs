//! The clauses that read a stored value — `FILTER` and `SORT` — with
//! their shapes and parsers. Split from `args.rs` for the 500-LOC house
//! rule (a `#[path]` child of `args`).

use super::MatchArgs;

/// One `FILTER` predicate: which stored value field it reads, and the
/// test on it. The shapes are the index query grammar's own `RANGE` /
/// `EQ`, rather than a second expression language invented for text.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FilterArg {
    pub(crate) field: Vec<u8>,
    pub(crate) shape: FilterShape,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum FilterShape {
    Range { min: Vec<u8>, max: Vec<u8> },
    Eq { value: Vec<u8> },
}

/// Parse one `FILTER <field> RANGE <min> <max>` / `FILTER <field> EQ <v>`
/// starting at the `FILTER` keyword; returns the clause and the next
/// index. Shared by the MATCH and the scalar grammars, so the two
/// surfaces cannot drift on what a predicate looks like.
pub(crate) fn parse_filter(argv: &[Vec<u8>], i: usize) -> Option<(FilterArg, usize)> {
    let field = argv.get(i + 1)?.clone();
    let mode = argv.get(i + 2)?;
    if mode.eq_ignore_ascii_case(b"RANGE") {
        let shape =
            FilterShape::Range { min: argv.get(i + 3)?.clone(), max: argv.get(i + 4)?.clone() };
        Some((FilterArg { field, shape }, i + 5))
    } else if mode.eq_ignore_ascii_case(b"EQ") {
        let shape = FilterShape::Eq { value: argv.get(i + 3)?.clone() };
        Some((FilterArg { field, shape }, i + 4))
    } else {
        None
    }
}

/// `ASC` = false, `DESC` = true. The direction is required: a default
/// would have to be one of them, and guessing which is a ranking
/// decision.
pub(crate) fn parse_sort_dir(dir: &[u8]) -> Option<bool> {
    if dir.eq_ignore_ascii_case(b"DESC") {
        Some(true)
    } else if dir.eq_ignore_ascii_case(b"ASC") {
        Some(false)
    } else {
        None
    }
}

/// [`parse_filter`] into a MATCH's clause list. Several FILTER clauses
/// AND together, which is why each appends rather than replaces.
pub(crate) fn apply_filter(argv: &[Vec<u8>], i: usize, a: &mut MatchArgs) -> Option<usize> {
    let (f, next) = parse_filter(argv, i)?;
    a.filters.push(f);
    Some(next)
}

/// The clauses that name a stored value field: `SORT`, `DISTINCT`,
/// `FACET`. Grouped because they share a shape — a field name, sometimes
/// with a modifier — and because it keeps the main clause dispatcher
/// readable as the surface grows.
pub(crate) fn apply_value_clause(argv: &[Vec<u8>], i: usize, a: &mut MatchArgs) -> Option<usize> {
    let kw = &argv[i];
    if kw.eq_ignore_ascii_case(b"SORT") {
        apply_sort(argv, i, a)
    } else if kw.eq_ignore_ascii_case(b"DISTINCT") {
        a.distinct = Some(argv.get(i + 1)?.clone());
        Some(i + 2)
    } else {
        let (fs, next) = super::collect_clause(argv, i + 1);
        if fs.is_empty() {
            return None;
        }
        a.facets = fs;
        Some(next)
    }
}

/// `SORT <field> ASC|DESC`.
fn apply_sort(argv: &[Vec<u8>], i: usize, a: &mut MatchArgs) -> Option<usize> {
    let field = argv.get(i + 1)?.clone();
    let desc = parse_sort_dir(argv.get(i + 2)?)?;
    a.sort = Some((field, desc));
    Some(i + 3)
}

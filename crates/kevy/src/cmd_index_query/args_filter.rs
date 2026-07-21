//! The `FILTER` clause's shapes and parser, split from `args.rs` for the
//! 500-LOC house rule (a `#[path]` child of `args`).

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
/// and return the next clause index. Several FILTER clauses AND together,
/// which is why each appends rather than replaces.
pub(crate) fn apply_filter(argv: &[Vec<u8>], i: usize, a: &mut MatchArgs) -> Option<usize> {
    let field = argv.get(i + 1)?.clone();
    let mode = argv.get(i + 2)?;
    if mode.eq_ignore_ascii_case(b"RANGE") {
        let shape = FilterShape::Range {
            min: argv.get(i + 3)?.clone(),
            max: argv.get(i + 4)?.clone(),
        };
        a.filters.push(FilterArg { field, shape });
        Some(i + 5)
    } else if mode.eq_ignore_ascii_case(b"EQ") {
        let shape = FilterShape::Eq { value: argv.get(i + 3)?.clone() };
        a.filters.push(FilterArg { field, shape });
        Some(i + 4)
    } else {
        None
    }
}


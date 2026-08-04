//! The observation hook of the auto-declaration loop: derive the
//! refused declaration family from `(argv, refusal chunk)` at the
//! origin reduce and feed the shared log ([`kevy_index::AdviseLog`]
//! via [`CatalogState`]). The derivation reads the argv the way the
//! parser would, minus the value coercion — a mis-derived family
//! costs one log seat, never a wrong answer.

use kevy_index::AdviseShape;

use crate::cmd_index_query::{ST_NOFIELD, ST_NOINDEX};
use crate::state::CatalogState;

/// Feed one refusal into the advise log, when its shape is one the
/// declaration surface can serve. Called once per refused query at
/// the origin — never per shard, so the reduce is the natural dedup.
pub(super) fn observe_refusal(catalogs: &CatalogState, argv: &[Vec<u8>], chunks: &[Vec<u8>]) {
    // HYBRID names two indexes and the chunk does not say which one
    // is missing, so it stays unadvised.
    if argv.get(1).is_some_and(|a| a.eq_ignore_ascii_case(b"HYBRID")) {
        return;
    }
    let Some(name) = argv.get(1) else { return };
    for c in chunks {
        match c.first().copied() {
            Some(ST_NOINDEX) => {
                if let Some(shape) = noindex_shape(argv) {
                    catalogs.advise_observe(name, shape, argv);
                }
                return;
            }
            Some(ST_NOFIELD) => {
                let flen = c.get(1).copied().unwrap_or(0) as usize;
                if let Some(field) = c.get(2..2 + flen) {
                    catalogs.advise_observe(name, AdviseShape::Filter(field.to_vec()), argv);
                }
                return;
            }
            _ => {}
        }
    }
}

/// The declaration family a NOINDEX refusal asked for, read from the
/// argv shape. `None` = a shape the declaration surface cannot serve
/// (KNN, GROUPS, COMPOSE, …), which is not logged.
fn noindex_shape(argv: &[Vec<u8>]) -> Option<AdviseShape> {
    let mode = argv.get(2)?;
    if mode.eq_ignore_ascii_case(b"MATCH") {
        return Some(AdviseShape::Match);
    }
    if mode.eq_ignore_ascii_case(b"RANGE") || mode.eq_ignore_ascii_case(b"EQ") {
        return Some(AdviseShape::Range);
    }
    if mode.eq_ignore_ascii_case(b"WHERE") {
        let cols = where_columns(&argv[3..]);
        return (!cols.is_empty()).then_some(AdviseShape::Where(cols));
    }
    None
}

/// The columns a WHERE clause names, in clause order: `col EQ v`
/// groups, then an optional `RANGE col min max` tail — the same walk
/// the parser does.
fn where_columns(rest: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut cols = Vec::new();
    let mut i = 0;
    while i + 2 < rest.len() && rest[i + 1].eq_ignore_ascii_case(b"EQ") {
        cols.push(rest[i].clone());
        i += 3;
    }
    if rest.get(i).is_some_and(|t| t.eq_ignore_ascii_case(b"RANGE"))
        && let Some(col) = rest.get(i + 1)
    {
        cols.push(col.clone());
    }
    cols
}

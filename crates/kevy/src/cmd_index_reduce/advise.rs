//! The observation hook of the auto-declaration loop: derive the
//! refused declaration family from `(argv, refusal chunk)` at the
//! origin reduce and feed the shared log ([`kevy_index::AdviseLog`]
//! via [`CatalogState`]). The derivation reads the argv the way the
//! parser would, minus the value coercion — a mis-derived family
//! costs one log seat, never a wrong answer.

use kevy_index::{AUTODECLARE_AFTER, AdviseEntry, AdviseShape};

use crate::cmd_index_query::{ST_NOFIELD, ST_NOINDEX};
use crate::state::{CatalogBase, CatalogChange, CatalogState, RuntimeState};

/// The whole refusal side: observe the family, then let the auto
/// loop act on the fresh count. One call from the reduce.
pub(super) fn on_refused(state: &RuntimeState, argv: &[Vec<u8>], chunks: &[Vec<u8>]) {
    if let Some((shape, count)) = observe_refusal(&state.catalogs, argv, chunks)
        && let Some(name) = argv.get(1)
    {
        maybe_autodeclare(state, name, shape, count);
    }
}

/// Feed one refusal into the advise log, when its shape is one the
/// declaration surface can serve. Called once per refused query at
/// the origin — never per shard, so the reduce is the natural dedup.
/// Returns the observed family and its count, the auto loop's input.
fn observe_refusal(
    catalogs: &CatalogState,
    argv: &[Vec<u8>],
    chunks: &[Vec<u8>],
) -> Option<(AdviseShape, u64)> {
    // HYBRID names two indexes and the chunk does not say which one
    // is missing, so it stays unadvised.
    if argv.get(1).is_some_and(|a| a.eq_ignore_ascii_case(b"HYBRID")) {
        return None;
    }
    let name = argv.get(1)?;
    for c in chunks {
        match c.first().copied() {
            Some(ST_NOINDEX) => {
                let shape = noindex_shape(argv)?;
                let count = catalogs.advise_observe(name, shape.clone(), argv);
                return Some((shape, count));
            }
            Some(ST_NOFIELD) => {
                let flen = c.get(1).copied().unwrap_or(0) as usize;
                let field = c.get(2..2 + flen)?;
                let shape = AdviseShape::Filter(field.to_vec());
                let count = catalogs.advise_observe(name, shape.clone(), argv);
                return Some((shape, count));
            }
            _ => {}
        }
    }
    None
}

/// The declare-period action: when the just-observed family crossed
/// the threshold and its table opted in with spare budget, apply the
/// declaration NOW. The refusal path is cold — the query that pushed
/// the count over still gets its error, and the next one finds the
/// path building. Failures leave everything unchanged: this is an
/// engine courtesy, never a correctness surface.
fn maybe_autodeclare(state: &RuntimeState, name: &[u8], shape: AdviseShape, count: u64) {
    if count < AUTODECLARE_AFTER {
        return;
    }
    let dot = match name.iter().position(|&b| b == b'.') {
        Some(d) => d,
        None => return,
    };
    let entry = AdviseEntry::new(name, shape, count);
    loop {
        let base = state.catalog_base();
        let Some(change) = autodeclared(&base, &name[..dot], &entry) else { return };
        if state.commit_catalogs(&base, change) {
            return;
        }
    }
}

/// The catalogs with the path `entry` asks for declared on table
/// `table`; `None` when there is nothing to declare.
fn autodeclared(base: &CatalogBase, table: &[u8], entry: &AdviseEntry) -> Option<CatalogChange> {
    let mut spec = base.table.as_deref()?.get(table).cloned()?;
    if spec.autodeclare == 0 {
        return None;
    }
    let ledger = spec.apply_auto(entry)?;
    let compiled = spec.compile().ok()?;
    let mut new_tcat = base.table_owned();
    new_tcat.drop_table(&spec.name);
    new_tcat.create(spec).ok()?;
    let mut icat = base.index_owned();
    // `path` = a whole new compiled index; `path#field` = a changed
    // one, rebuilt (drop + create) so the VALUES payloads backfill.
    let path = match ledger.iter().position(|&b| b == b'#') {
        Some(p) => &ledger[..p],
        None => &ledger[..],
    };
    let ispec = compiled.into_iter().find(|s| s.name() == path)?;
    // a path rebuilt for its new VALUES keeps how it was spread
    let part = icat.partitioning(path).clone();
    icat.drop_index(path);
    icat.create_with(ispec, part).ok()?;
    Some(CatalogChange { index: Some(icat), table: Some(new_tcat), view: None })
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

/// The observation's dual — count one SERVED query against its
/// path's usage cell. Same convergence point as [`observe_refusal`]
/// (the origin, once per query); only the queries a human aims at a
/// named path count, not the internal second-phase verbs.
pub(super) fn observe_hit(catalogs: &CatalogState, argv: &[Vec<u8>]) {
    let verb = argv.first().map(Vec::as_slice).unwrap_or(b"");
    if !verb.eq_ignore_ascii_case(b"IDX.QUERY") && !verb.eq_ignore_ascii_case(b"IDX.COUNT") {
        return;
    }
    let now_s = (kevy_store::now_unix_ms() / 1000) as i64;
    // HYBRID serves through both of its named indexes.
    let names: &[usize] =
        if argv.get(1).is_some_and(|a| a.eq_ignore_ascii_case(b"HYBRID")) { &[2, 4] } else { &[1] };
    for &i in names {
        if let Some(name) = argv.get(i)
            && let Some(cell) = catalogs.usage_cell(name)
        {
            cell.hit(now_s);
        }
    }
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

#[cfg(test)]
#[path = "advise_tests.rs"]
mod tests;

//! `sql run [--max-rows N] '<SELECT …>'`: one SELECT, planned by kevy-sql
//! against the tables the server declares (read back with
//! `TABLE.DESCRIBE`), sent as the `IDX.QUERY` that answers it. The engine
//! never sees SQL; a query no declared path serves is refused with the
//! text `sql plan` gives it.

use super::catalog::names;
use super::described::{self, Kind};
use super::options::Common;
use super::query::{Paging, collect};
use super::render::render;
use super::rows::{Cell, Rows};
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};

/// Run `sql run`; the exit code.
pub(crate) fn run(s: &mut Session, common: &Common) -> u8 {
    let Some((paging, words)) = super::query::paging(&common.args) else { return 1 };
    let [select] = words.as_slice() else {
        eprint_bytes(&[b"usage: kevy-cli sql run [--max-rows n] 'SELECT ... FROM t WHERE ...'\n"]);
        return 1;
    };
    let Some(select) = std::str::from_utf8(select).ok() else {
        eprint_bytes(&[b"kevy-cli: sql run: the statement is not UTF-8\n"]);
        return 1;
    };
    let Some(declarations) = declarations(s) else { return 1 };
    let card = match kevy_sql::select_card(&declarations, select) {
        Ok(card) => card,
        Err(e) => {
            eprint_bytes(&[b"kevy-cli: sql run: ", e.to_string().as_bytes(), b"\n"]);
            return 1;
        }
    };
    let argv: Vec<Vec<u8>> = card.argv.iter().map(|w| w.clone().into_bytes()).collect();
    let limited = card.argv.iter().any(|w| w == "LIMIT");
    let paging = Paging { all: !limited, max_rows: paging.max_rows };
    let Some(mut collected) = collect(s, &argv, &paging) else { return 1 };
    let selected = card.argv.iter().skip_while(|w| *w != "FIELDS").skip(1);
    project(&mut collected.rows, &selected.map(|w| w.as_bytes()).collect::<Vec<_>>());
    write_out(&render(&collected.rows, &common.style));
    if collected.stopped_at.is_some() {
        let n = collected.rows.rows.len();
        eprint_bytes(&[format!(
            "kevy-cli: stopped after {n} rows (--max-rows); add LIMIT or raise --max-rows\n"
        )
        .as_bytes()]);
    }
    0
}

/// Keep only the SELECT list's columns, in its order: the key and the index
/// value an `IDX.QUERY` page carries are not columns the statement named.
fn project(rows: &mut Rows, selected: &[&[u8]]) {
    let at: Vec<Option<usize>> =
        selected.iter().map(|c| rows.columns.iter().position(|have| have == c)).collect();
    rows.columns = selected.iter().map(|c| c.to_vec()).collect();
    for row in &mut rows.rows {
        *row = at.iter().map(|i| i.map_or(Cell::Null, |i| row[i].clone())).collect();
    }
}

/// Every declared table's `TABLE.DECLARE` argv, as the compiler reads it.
fn declarations(s: &mut Session) -> Option<Vec<Vec<String>>> {
    let Some(tables) = names(s, b"TABLE.LIST") else {
        eprint_bytes(&[b"kevy-cli: sql run: the server did not answer TABLE.LIST with rows\n"]);
        return None;
    };
    let mut out = Vec::with_capacity(tables.len());
    for table in tables {
        // A table dropped since TABLE.LIST is simply not there to plan on.
        let Some(d) = described::fetch(s, Kind::Table, &table).ok()? else { continue };
        let argv = d.declaration().unwrap_or_default();
        out.push(argv.into_iter().map(|w| String::from_utf8_lossy(&w).into_owned()).collect());
    }
    Some(out)
}

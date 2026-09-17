//! `tables`, `indexes`, `views`: the catalogs, as rows, optionally narrowed
//! by a glob on the name (or, for indexes, a table's name).

use super::options::Common;
use super::render::render;
use super::route::{Tool, ask};
use super::rows::{Cell, Rows, from_pair_rows};
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};

/// List one catalog; the exit code.
pub(crate) fn list(s: &mut Session, tool: Tool, common: &Common) -> u8 {
    let verb: &[u8] = match tool {
        Tool::Indexes => b"IDX.LIST",
        Tool::Views => b"VIEW.LIST",
        _ => b"TABLE.LIST",
    };
    let Some(reply) = ask(s, &[verb]) else { return 1 };
    let Some(mut rows) = from_pair_rows(&reply) else {
        eprint_bytes(&[b"kevy-cli: ", verb, b" replied in a shape this tool does not read\n"]);
        return 1;
    };
    if tool == Tool::Indexes {
        add_table_column(&mut rows);
    }
    if let Some(pattern) = common.args.first() {
        rows.rows.retain(|row| matches(&rows.columns, row, pattern, tool));
    }
    write_out(&render(&rows, &common.style));
    0
}

/// `table`, from an index named `<table>.<path>`, right after `name`.
fn add_table_column(rows: &mut Rows) {
    let Some(name) = rows.columns.iter().position(|c| c == b"name") else { return };
    rows.columns.insert(name + 1, b"table".to_vec());
    for row in &mut rows.rows {
        let table = match &row[name] {
            Cell::Text(n) => n
                .iter()
                .position(|&b| b == b'.')
                .map_or(Cell::Null, |dot| Cell::Text(n[..dot].to_vec())),
            _ => Cell::Null,
        };
        row.insert(name + 1, table);
    }
}

/// The row's name matches the glob — or, for an index, its table's name
/// equals the pattern.
fn matches(columns: &[Vec<u8>], row: &[Cell], pattern: &[u8], tool: Tool) -> bool {
    let field = |name: &[u8]| {
        columns.iter().position(|c| c == name).and_then(|i| match &row[i] {
            Cell::Text(t) => Some(t.as_slice()),
            _ => None,
        })
    };
    let by_table = tool == Tool::Indexes && field(b"table") == Some(pattern);
    by_table || field(b"name").is_some_and(|n| kevy_store::glob_match(pattern, n))
}

//! `describe <table>` / `describe+`: the table's declaration as far as the
//! server tells it, its access paths, and (with `+`) its verification.

use super::options::Common;
use super::render::render;
use super::route::ask;
use super::rows::{Cell, Rows, from_pair_rows};
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};

/// Describe one table; the exit code.
pub(crate) fn run(s: &mut Session, plus: bool, common: &Common) -> u8 {
    let Some(table) = common.args.first() else {
        eprint_bytes(&[b"kevy-cli: describe needs a table name\n"]);
        return 1;
    };
    let Some(tables) = ask(s, &[b"TABLE.LIST"]).as_ref().and_then(from_pair_rows) else { return 1 };
    let mut header = only(&tables, |name| name == table.as_slice());
    if header.rows.is_empty() {
        eprint_bytes(&[b"kevy-cli: no such table '", table, b"' (tables lists them)\n"]);
        return 1;
    }
    drop_column(&mut header, b"name");
    section(&[&b"Table \""[..], table, b"\""].concat(), &header, common);
    let Some(indexes) = ask(s, &[b"IDX.LIST"]).as_ref().and_then(from_pair_rows) else { return 1 };
    let prefix = [table.as_slice(), b"."].concat();
    section(b"Access paths", &only(&indexes, |name| name.starts_with(&prefix)), common);
    write_out(
        b"Columns and their types are not readable over the wire until TABLE.DESCRIBE exists.\n",
    );
    if !plus {
        return 0;
    }
    let Some(verify) = ask(s, &[b"TABLE.VERIFY", table]).as_ref().and_then(from_pair_rows) else {
        return 1;
    };
    section(b"Verification", &verify, common);
    0
}

/// The rows whose `name` passes `keep`.
fn only(rows: &Rows, keep: impl Fn(&[u8]) -> bool) -> Rows {
    let name = rows.columns.iter().position(|c| c == b"name");
    let rows_kept = rows
        .rows
        .iter()
        .filter(|row| {
            name.and_then(|i| match &row[i] {
                Cell::Text(t) => Some(keep(t)),
                _ => None,
            }) == Some(true)
        })
        .cloned()
        .collect();
    Rows { columns: rows.columns.clone(), rows: rows_kept }
}

fn drop_column(rows: &mut Rows, name: &[u8]) {
    if let Some(i) = rows.columns.iter().position(|c| c == name) {
        rows.columns.remove(i);
        rows.rows.iter_mut().for_each(|r| {
            r.remove(i);
        });
    }
}

fn section(title: &[u8], rows: &Rows, common: &Common) {
    write_out(&[title, b"\n"].concat());
    write_out(&render(rows, &common.style));
}

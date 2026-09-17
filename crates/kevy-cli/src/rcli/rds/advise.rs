//! `advise`: the paths refused queries asked for, as the commands that would
//! declare them, and the paths nothing has used.

use super::options::Common;
use super::render::render;
use super::route::ask;
use super::rows::{Cell, Rows, from_advise};
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};

/// Run `advise`; the exit code.
pub(crate) fn run(s: &mut Session, common: &Common) -> u8 {
    let Some(reply) = ask(s, &[b"IDX.ADVISE"]) else { return 1 };
    let Some(rows) = from_advise(&reply) else {
        eprint_bytes(&[b"kevy-cli: IDX.ADVISE replied in a shape this tool does not read\n"]);
        return 1;
    };
    let unused = |row: &Vec<Cell>| matches!(&row[2], Cell::Text(c) if c.starts_with(b"IDX.DROP"));
    let (never, missing): (Vec<Vec<Cell>>, Vec<Vec<Cell>>) =
        rows.rows.iter().cloned().partition(unused);
    write_out(b"Refused queries would be served by\n");
    write_out(&render(&Rows { columns: rows.columns.clone(), rows: missing }, &common.style));
    write_out(b"Never used\n");
    write_out(&render(&Rows { columns: rows.columns, rows: never }, &common.style));
    0
}

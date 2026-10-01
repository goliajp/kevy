//! `describe <name>` / `describe+`: a table's columns and access paths, an
//! index's fields and options, or a view's composition — read from the
//! DESCRIBE verbs — and with `+` the object's verification.

use super::described::{self, Described, Kind, words};
use super::options::Common;
use super::render::render;
use super::route::ask;
use super::rows::{Cell, Rows, from_pair_rows};
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};
use kevy_resp::Reply;

/// Describe one table, index or view; the exit code.
pub(crate) fn run(s: &mut Session, plus: bool, common: &Common) -> u8 {
    let Some(name) = common.args.first() else {
        eprint_bytes(&[b"kevy-cli: describe needs a table, index or view name\n"]);
        return 1;
    };
    let d = match described::find(s, name) {
        Ok(Some(d)) => d,
        Ok(None) => {
            eprint_bytes(&[b"kevy-cli: no table, index or view named '", name, b"'\n"]);
            return 1;
        }
        Err(()) => return 1,
    };
    let shown = match d.kind {
        Kind::Table => table(s, name, &d, common),
        Kind::Index => index(name, &d, common),
        Kind::View => view(name, &d, common),
    };
    if !shown || !plus {
        return u8::from(!shown);
    }
    let verb: &[u8] = match d.kind {
        Kind::Table => b"TABLE.VERIFY",
        Kind::Index => b"IDX.VERIFY",
        Kind::View => b"VIEW.VERIFY",
    };
    let Some(verify) = ask(s, &[verb, name]).as_ref().and_then(from_pair_rows) else { return 1 };
    section(b"Verification", &verify, common);
    0
}

fn table(s: &mut Session, name: &[u8], d: &Described, common: &Common) -> bool {
    let header = one_row(
        d,
        &[b"prefix", b"pk", b"autodeclare"],
        &[(b"window", pairs_text(d.field(b"window")))],
    );
    section(&title(b"Table", name), &header, common);
    let paths = column_paths(d);
    let pk = d.text(b"pk").unwrap_or_default().to_vec();
    let mut columns = rows(&[b"column", b"type", b"key", b"paths"]);
    for (column, ty) in d.columns() {
        let key = if column == pk { b"pk".to_vec() } else { Vec::new() };
        let on: Vec<&[u8]> =
            paths.iter().filter(|(c, _)| *c == column).map(|(_, p)| p.as_slice()).collect();
        columns.rows.push(vec![text(column), text(ty), text(key), text(on.join(&b", "[..]))]);
    }
    section(b"Columns", &columns, common);
    let Some(indexes) = ask(s, &[b"IDX.LIST"]).as_ref().and_then(from_pair_rows) else {
        return false;
    };
    let prefix = [name, b"."].concat();
    section(b"Access paths", &only(&indexes, |n| n.starts_with(&prefix)), common);
    true
}

/// `(column, path)` for every compiled path that reads the column.
fn column_paths(d: &Described) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in entries(d, b"indexes") {
        if let (Some(path), Some(column)) = (label(&entry, b"path"), label(&entry, b"column")) {
            out.push((column.to_vec(), path.to_vec()));
        }
    }
    for entry in entries(d, b"orderpaths") {
        let Some(path) = label(&entry, b"path") else { continue };
        let Some(Reply::Array(on)) = value(&entry, b"on") else { continue };
        for pair in on.iter().filter_map(words) {
            if let Some(column) = pair.first() {
                out.push((column.clone(), path.to_vec()));
            }
        }
    }
    out
}

fn index(name: &[u8], d: &Described, common: &Common) -> bool {
    let header = one_row(
        d,
        &[b"prefix", b"kind", b"type", b"table", b"positions", b"maxmem", b"groupby"],
        &[(b"ann", pairs_text(d.field(b"ann")))],
    );
    section(&title(b"Index", name), &header, common);
    section(b"Fields", &listed(d.field(b"fields"), &[b"field", b"weight"]), common);
    for (label, title, columns) in [
        (&b"values"[..], &b"Stored values"[..], &[&b"value"[..], b"type"][..]),
        (b"composite", b"Composite", &[b"column", b"type", b"order"]),
    ] {
        let rows = listed(d.field(label), columns);
        if !rows.rows.is_empty() {
            section(title, &rows, common);
        }
    }
    true
}

fn view(name: &[u8], d: &Described, common: &Common) -> bool {
    let query = d.field(b"query").and_then(words).unwrap_or_default();
    let header = one_row(
        d,
        &[b"order_by", b"desc", b"mode", b"topk", b"via"],
        &[(b"query", described::command_line(&query))],
    );
    section(&title(b"View", name), &header, common);
    true
}

fn title(noun: &[u8], name: &[u8]) -> Vec<u8> {
    [noun, b" \"", name, b"\""].concat()
}

fn text(t: impl AsRef<[u8]>) -> Cell {
    Cell::Text(t.as_ref().to_vec())
}

fn rows(columns: &[&[u8]]) -> Rows {
    Rows { columns: columns.iter().map(|c| c.to_vec()).collect(), rows: Vec::new() }
}

/// One row of the named text fields, then the extra computed cells.
fn one_row(d: &Described, labels: &[&[u8]], extra: &[(&[u8], Vec<u8>)]) -> Rows {
    let mut r = rows(labels);
    let mut row: Vec<Cell> = labels.iter().map(|l| d.text(l).map_or(Cell::Null, text)).collect();
    for (label, cell) in extra {
        r.columns.push(label.to_vec());
        row.push(text(cell));
    }
    r.rows.push(row);
    r
}

/// A nested label/value array as `label=value …`; a bare `-` as itself.
fn pairs_text(reply: Option<&Reply>) -> Vec<u8> {
    match reply {
        Some(Reply::Array(items)) => items
            .chunks(2)
            .filter_map(|kv| Some([first(&kv[0])?, b"=", first(kv.get(1)?)?].concat()))
            .collect::<Vec<_>>()
            .join(&b" "[..]),
        Some(Reply::Bulk(b)) => b.clone(),
        _ => Vec::new(),
    }
}

fn first(reply: &Reply) -> Option<&[u8]> {
    match reply {
        Reply::Bulk(b) => Some(b),
        _ => None,
    }
}

/// An array of word arrays as rows under `columns`.
fn listed(reply: Option<&Reply>, columns: &[&[u8]]) -> Rows {
    let mut r = rows(columns);
    if let Some(Reply::Array(items)) = reply {
        r.rows.extend(items.iter().filter_map(words).map(|w| w.into_iter().map(text).collect()));
    }
    r
}

/// The label/value arrays inside `label`'s array.
fn entries(d: &Described, label: &[u8]) -> Vec<Vec<Reply>> {
    match d.field(label) {
        Some(Reply::Array(items)) => items
            .iter()
            .filter_map(|i| if let Reply::Array(kv) = i { Some(kv.clone()) } else { None })
            .collect(),
        _ => Vec::new(),
    }
}

fn value<'a>(entry: &'a [Reply], want: &[u8]) -> Option<&'a Reply> {
    let at = entry.chunks(2).position(|kv| first(&kv[0]) == Some(want))?;
    entry.get(at * 2 + 1)
}

fn label<'a>(entry: &'a [Reply], want: &[u8]) -> Option<&'a [u8]> {
    value(entry, want).and_then(first)
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

fn section(title: &[u8], rows: &Rows, common: &Common) {
    write_out(&[title, b"\n"].concat());
    write_out(&render(rows, &common.style));
}

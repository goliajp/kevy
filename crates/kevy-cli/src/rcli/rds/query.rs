//! `query [--all] [--max-rows N] <IDX.QUERY|VIEW.QUERY …>`: a query's rows,
//! following its cursor page by page when asked.

use super::options::Common;
use super::render::render;
use super::rows::{Rows, from_query};
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};
use kevy_resp::Reply;

/// Paging choices.
pub(crate) struct Paging {
    pub(crate) all: bool,
    pub(crate) max_rows: usize,
}

/// What running a query collected.
pub(crate) struct Collected {
    pub(crate) rows: Rows,
    pub(crate) pages: usize,
    /// The cursor to continue from when `--max-rows` stopped it.
    pub(crate) stopped_at: Option<Vec<u8>>,
}

/// Run `query`; the exit code.
pub(crate) fn run(s: &mut Session, common: &Common) -> u8 {
    let Some((paging, argv)) = paging(&common.args) else { return 1 };
    let Some(collected) = collect(s, &argv, &paging) else { return 1 };
    write_out(&render(&collected.rows, &common.style));
    if let Some(cursor) = collected.stopped_at {
        let n = collected.rows.rows.len();
        eprint_bytes(&[
            format!("kevy-cli: stopped after {n} rows (--max-rows); continue with CURSOR ")
                .as_bytes(),
            &cursor,
            b"\n",
        ]);
    }
    0
}

/// `--all` and `--max-rows N` before the verb; the verb's words.
pub(crate) fn paging(args: &[Vec<u8>]) -> Option<(Paging, Vec<Vec<u8>>)> {
    let mut paging = Paging { all: false, max_rows: 10_000 };
    let mut i = 0;
    loop {
        match args.get(i).map(Vec::as_slice) {
            Some(b"--all") => paging.all = true,
            Some(b"--max-rows") => {
                let n = args.get(i + 1).and_then(|v| std::str::from_utf8(v).ok()?.parse().ok());
                let Some(n) = n else {
                    eprint_bytes(&[b"kevy-cli: --max-rows needs a number\n"]);
                    return None;
                };
                paging.max_rows = n;
                i += 1;
            }
            _ => break,
        }
        i += 1;
    }
    if i >= args.len() {
        eprint_bytes(&[
            b"kevy-cli: query needs a query verb (IDX.QUERY or VIEW.QUERY) and its arguments\n",
        ]);
        return None;
    }
    Some((paging, args[i..].to_vec()))
}

/// Run the query, following the cursor when `paging.all`.
pub(crate) fn collect(s: &mut Session, argv: &[Vec<u8>], paging: &Paging) -> Option<Collected> {
    let value_column: &[u8] =
        if argv[0].eq_ignore_ascii_case(b"VIEW.QUERY") { b"order_value" } else { b"value" };
    let mut words = argv.to_vec();
    let mut out = Collected { rows: Rows::default(), pages: 0, stopped_at: None };
    loop {
        let refs: Vec<&[u8]> = words.iter().map(Vec::as_slice).collect();
        let reply = match s.request(&refs) {
            Ok(Reply::Error(msg)) => return refused(&msg),
            Ok(reply) => reply,
            Err(e) => {
                eprint_bytes(&[b"Error: ", e.text().as_bytes(), b"\n"]);
                return None;
            }
        };
        let Some((cursor, page)) = from_query(&reply, value_column) else {
            eprint_bytes(&[b"kevy-cli: the reply is not a query page (IDX.QUERY or VIEW.QUERY)\n"]);
            return None;
        };
        append(&mut out.rows, page);
        out.pages += 1;
        if cursor == b"0" || !paging.all {
            return Some(out);
        }
        // Whole pages only: the cursor continues after the last row shown.
        if out.rows.rows.len() >= paging.max_rows {
            out.stopped_at = Some(cursor);
            return Some(out);
        }
        set_cursor(&mut words, cursor);
    }
}

fn append(all: &mut Rows, page: Rows) {
    if all.columns.is_empty() {
        *all = page;
        return;
    }
    for (row, _) in page.rows.into_iter().zip(0..) {
        let mut aligned = vec![super::rows::Cell::Null; all.columns.len()];
        for (i, cell) in row.into_iter().enumerate() {
            if let Some(name) = page.columns.get(i) {
                let at = match all.columns.iter().position(|c| c == name) {
                    Some(at) => at,
                    None => {
                        all.columns.push(name.clone());
                        all.rows.iter_mut().for_each(|r| r.push(super::rows::Cell::Null));
                        aligned.push(super::rows::Cell::Null);
                        all.columns.len() - 1
                    }
                };
                aligned[at] = cell;
            }
        }
        all.rows.push(aligned);
    }
}

/// Put `CURSOR c` where the verb reads it: replacing a given cursor, else
/// before `FIELDS` (which takes every word after it), else at the end.
fn set_cursor(words: &mut Vec<Vec<u8>>, cursor: Vec<u8>) {
    let at = |word: &[u8]| {
        words.iter().skip(2).position(|w| w.eq_ignore_ascii_case(word)).map(|i| i + 2)
    };
    match (at(b"CURSOR"), at(b"FIELDS")) {
        (Some(i), _) if i + 1 < words.len() => words[i + 1] = cursor,
        (_, Some(f)) => {
            words.insert(f, cursor);
            words.insert(f, b"CURSOR".to_vec());
        }
        _ => words.extend([b"CURSOR".to_vec(), cursor]),
    }
}

/// The engine's refusal as it came, and where to look for a path.
fn refused<T>(msg: &[u8]) -> Option<T> {
    eprint_bytes(&[b"(error) ", msg, b"\n"]);
    if !msg.starts_with(b"INDEXBUILDING") {
        eprint_bytes(&[b"kevy-cli: `kevy-cli advise` lists the paths refused queries asked for; `kevy-cli sql plan` checks a query against a schema\n"]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::set_cursor;

    fn words(s: &str) -> Vec<Vec<u8>> {
        s.split(' ').map(|w| w.as_bytes().to_vec()).collect()
    }

    #[test]
    fn the_cursor_goes_where_the_verb_reads_it() {
        let mut w = words("IDX.QUERY i RANGE 0 9 LIMIT 2 FIELDS a b");
        set_cursor(&mut w, b"c1".to_vec());
        assert_eq!(w, words("IDX.QUERY i RANGE 0 9 LIMIT 2 CURSOR c1 FIELDS a b"));
        set_cursor(&mut w, b"c2".to_vec());
        assert_eq!(w, words("IDX.QUERY i RANGE 0 9 LIMIT 2 CURSOR c2 FIELDS a b"));
        let mut v = words("VIEW.QUERY v LIMIT 5");
        set_cursor(&mut v, b"x".to_vec());
        assert_eq!(v, words("VIEW.QUERY v LIMIT 5 CURSOR x"));
    }
}

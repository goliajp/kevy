//! `explain <index> <shape…>`, `explain view <name>`, and `explain
//! --analyze <query…>` — the last measured by this client, not the server.

use super::options::Common;
use super::query::{Paging, collect};
use super::render::render;
use super::route::ask;
use super::rows::{Cell, Rows, from_pair_list};
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};

/// Run `explain`; the exit code.
pub(crate) fn run(s: &mut Session, common: &Common) -> u8 {
    match common.args.split_first() {
        Some((flag, query)) if flag == b"--analyze" => analyze(s, query, common),
        Some((word, rest)) if word == b"view" && rest.len() == 1 => {
            let Some(reply) = ask(s, &[b"VIEW.EXPLAIN", &rest[0]]) else { return 1 };
            write_out(&crate::rcli::format::render(
                &reply,
                &[],
                crate::rcli::format::Output::Standard,
                &s.opts.delims,
                false,
            ));
            0
        }
        Some((index, shape)) if !shape.is_empty() => {
            let mut argv: Vec<&[u8]> = vec![b"IDX.EXPLAIN", index];
            argv.extend(shape.iter().map(Vec::as_slice));
            let Some(rows) = ask(s, &argv).as_ref().and_then(from_pair_list) else { return 1 };
            write_out(&render(&rows, &common.style));
            0
        }
        _ => {
            eprint_bytes(&[b"kevy-cli: explain <index> <shape...> | explain view <name> | explain --analyze <query...>\n"]);
            1
        }
    }
}

/// Run the whole query once and report what this client observed.
fn analyze(s: &mut Session, query: &[Vec<u8>], common: &Common) -> u8 {
    if query.is_empty() {
        eprint_bytes(&[b"kevy-cli: explain --analyze needs a query verb and its arguments\n"]);
        return 1;
    }
    let started = std::time::Instant::now();
    let paging = Paging { all: true, max_rows: usize::MAX };
    let Some(collected) = collect(s, query, &paging) else { return 1 };
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    write_out(
        b"Client-side measurement (round trips from this client; not a server-side breakdown)\n",
    );
    let text = |t: String| Cell::Text(t.into_bytes());
    let rows = Rows {
        columns: ["rows", "pages", "elapsed_ms"].iter().map(|c| c.as_bytes().to_vec()).collect(),
        rows: vec![vec![
            Cell::Int(collected.rows.rows.len() as i64),
            Cell::Int(collected.pages as i64),
            text(format!("{elapsed:.3}")),
        ]],
    };
    write_out(&render(&rows, &common.style));
    0
}

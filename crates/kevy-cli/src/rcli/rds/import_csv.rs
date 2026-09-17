//! `import-csv <file> --prefix p --pk col [--header] [--columns a,b]
//! [--delimiter c] [--null-marker s] [--resume] [--strict]`: one hash per
//! record, `<prefix><pk>`, written in pipelined batches.
//!
//! An empty cell (or the null marker) writes no field: a missing field is
//! NULL. Progress is kept in `<file>.progress` as `import` keeps it.

use super::csv;
use super::options::Common;
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};
use kevy_resp::Reply;
use std::path::Path;

/// Records per pipelined batch.
const BATCH: usize = 512;

struct Plan {
    file: Vec<u8>,
    prefix: Vec<u8>,
    pk: Vec<u8>,
    header: bool,
    columns: Vec<Vec<u8>>,
    delimiter: u8,
    null_marker: Option<Vec<u8>>,
    resume: bool,
    strict: bool,
}

/// Run `import-csv`; the exit code.
pub(crate) fn run(s: &mut Session, common: &Common) -> u8 {
    let Some(mut plan) = parse(&common.args) else { return 1 };
    let path = std::path::PathBuf::from(String::from_utf8_lossy(&plan.file).into_owned());
    let text = match std::fs::read(&path) {
        Ok(t) => t,
        Err(e) => {
            return fail(&[
                b"cannot read '",
                &plan.file,
                b"': ",
                crate::rcli::conn::strerror(&e).as_bytes(),
            ]);
        }
    };
    let mut at = 0;
    if plan.header {
        let Some(head) = csv::next(&text, 0, plan.delimiter) else {
            return fail(&[b"the file has no header record"]);
        };
        plan.columns = head.fields;
        at = head.end;
    }
    let Some(pk_at) = plan.columns.iter().position(|c| *c == plan.pk) else {
        return fail(&[b"no column named '", &plan.pk, b"' (give --header or --columns)"]);
    };
    warn_if_declared(s, &plan.prefix);
    import(s, &plan, &path, &text, at, pk_at)
}

fn import(
    s: &mut Session,
    plan: &Plan,
    path: &Path,
    text: &[u8],
    header_end: usize,
    pk_at: usize,
) -> u8 {
    let (mut progress, start) = match crate::migrate::open_progress(path, plan.resume) {
        Ok(p) => p,
        Err(e) => {
            return fail(&[
                b"cannot write the progress file: ",
                crate::rcli::conn::strerror(&e).as_bytes(),
            ]);
        }
    };
    let (mut at, mut rows, mut errors) = (start.max(header_end as u64) as usize, 0u64, 0u64);
    loop {
        let (batch, end) = next_batch(plan, text, at, pk_at);
        if batch.is_empty() {
            break;
        }
        match send(s, plan, &batch) {
            Ok((sent, failed)) => (rows, errors) = (rows + sent, errors + failed),
            Err(code) => return code,
        }
        at = end;
        if let Err(e) = crate::migrate::write_progress(&mut progress, at as u64) {
            return fail(&[
                b"cannot write the progress file: ",
                crate::rcli::conn::strerror(&e).as_bytes(),
            ]);
        }
    }
    let prefix = String::from_utf8_lossy(&plan.prefix);
    write_out(format!("imported {rows} rows into {prefix} ({errors} errors)\n").as_bytes());
    if errors > 0 { 3 } else { 0 }
}

/// Up to BATCH commands from the records at `at`, and where they end.
fn next_batch(plan: &Plan, text: &[u8], at: usize, pk_at: usize) -> (Vec<Vec<Vec<u8>>>, usize) {
    let (mut batch, mut end) = (Vec::new(), at);
    while batch.len() < BATCH {
        let Some(record) = csv::next(text, end, plan.delimiter) else { break };
        end = record.end;
        if let Some(cmd) = command(plan, &record.fields, pk_at) {
            batch.push(cmd);
        }
    }
    (batch, end)
}

/// One pipelined batch: rows sent and error replies; `Err(exit code)` when
/// the link is lost or `--strict` stops at an error.
fn send(s: &mut Session, plan: &Plan, batch: &[Vec<Vec<u8>>]) -> Result<(u64, u64), u8> {
    let commands: Vec<Vec<&[u8]>> =
        batch.iter().map(|c| c.iter().map(Vec::as_slice).collect()).collect();
    let replies = s.pipeline(&commands).map_err(|e| {
        eprint_bytes(&[b"Error: ", e.text().as_bytes(), b"\n"]);
        2u8
    })?;
    let mut errors = 0;
    for msg in replies.iter().filter_map(|r| if let Reply::Error(m) = r { Some(m) } else { None }) {
        errors += 1;
        eprint_bytes(&[b"(error) ", msg, b"\n"]);
        if plan.strict {
            return Err(3);
        }
    }
    Ok((replies.len() as u64, errors))
}

/// `HSET <prefix><pk> col val …` for the non-empty cells; `None` for a
/// record without a key.
fn command(plan: &Plan, fields: &[Vec<u8>], pk_at: usize) -> Option<Vec<Vec<u8>>> {
    let pk = fields.get(pk_at).filter(|v| !v.is_empty())?;
    let mut cmd = vec![b"HSET".to_vec(), [plan.prefix.as_slice(), pk].concat()];
    for (name, value) in plan.columns.iter().zip(fields) {
        let null = value.is_empty() || plan.null_marker.as_ref() == Some(value);
        if !null {
            cmd.extend([name.clone(), value.clone()]);
        }
    }
    Some(cmd)
}

/// Declared indexes on the prefix update on every row written.
fn warn_if_declared(s: &mut Session, prefix: &[u8]) {
    let Ok(Reply::Array(indexes)) = s.request(&[b"IDX.LIST"]) else { return };
    let needle = super::rows::from_pair_rows(&Reply::Array(indexes)).is_some_and(|rows| {
        let at = rows.columns.iter().position(|c| c == b"prefix");
        rows.rows
            .iter()
            .any(|r| at.is_some_and(|i| r[i] == super::rows::Cell::Text(prefix.to_vec())))
    });
    if needle {
        eprint_bytes(&[
            b"kevy-cli: indexes are declared on '",
            prefix,
            b"'; each row updates them as it lands (importing before declaring is cheaper)\n",
        ]);
    }
}

fn fail(parts: &[&[u8]]) -> u8 {
    eprint_bytes(&[b"kevy-cli: import-csv: ", &parts.concat(), b"\n"]);
    1
}

fn parse(args: &[Vec<u8>]) -> Option<Plan> {
    let mut plan = Plan {
        file: Vec::new(),
        prefix: Vec::new(),
        pk: Vec::new(),
        header: false,
        columns: Vec::new(),
        delimiter: b',',
        null_marker: None,
        resume: false,
        strict: false,
    };
    let mut i = 0;
    while i < args.len() {
        let value = args.get(i + 1).cloned();
        match (args[i].as_slice(), value) {
            (b"--prefix", Some(v)) => plan.prefix = v,
            (b"--pk", Some(v)) => plan.pk = v,
            (b"--columns", Some(v)) => {
                plan.columns = v.split(|&b| b == b',').map(<[u8]>::to_vec).collect()
            }
            (b"--delimiter", Some(v)) if v.len() == 1 => plan.delimiter = v[0],
            (b"--null-marker", Some(v)) => plan.null_marker = Some(v),
            (flag, _) if flag.starts_with(b"--") => {
                match flag {
                    b"--header" => plan.header = true,
                    b"--resume" => plan.resume = true,
                    b"--strict" => plan.strict = true,
                    _ => return usage(flag),
                }
                i += 1;
                continue;
            }
            (file, _) if plan.file.is_empty() => {
                plan.file = file.to_vec();
                i += 1;
                continue;
            }
            (other, _) => return usage(other),
        }
        i += 2;
    }
    let ready = !plan.file.is_empty() && !plan.prefix.is_empty() && !plan.pk.is_empty();
    if !ready || (!plan.header && plan.columns.is_empty()) {
        return usage(b"");
    }
    Some(plan)
}

fn usage<T>(bad: &[u8]) -> Option<T> {
    if !bad.is_empty() {
        eprint_bytes(&[b"kevy-cli: import-csv: unexpected '", bad, b"'\n"]);
    }
    eprint_bytes(&[b"usage: kevy-cli import-csv <file> --prefix p --pk col (--header | --columns a,b,...) [--delimiter c] [--null-marker s] [--resume] [--strict]\n"]);
    None
}

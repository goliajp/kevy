//! `export-csv (--prefix p --columns a,b | --table t) [--via "<IDX.QUERY …>"]
//! <file|->`: hashes as CSV, key first. Without `--via` the keys come from
//! SCAN — a walk of the whole keyspace; with it, from the query's pages.
//! `--table` reads the prefix and, unless `--columns` names some, every
//! declared column from `TABLE.DESCRIBE`.

use super::csv;
use super::options::Common;
use super::query::{Paging, collect};
use super::rows::Cell;
use crate::rcli::session::{Session, eprint_bytes};
use kevy_resp::Reply;
use std::io::Write;

pub(crate) struct Plan {
    pub(crate) prefix: Vec<u8>,
    pub(crate) columns: Vec<Vec<u8>>,
    pub(crate) via: Option<Vec<Vec<u8>>>,
    pub(crate) file: Vec<u8>,
    table: Option<Vec<u8>>,
}

impl Plan {
    /// A SCAN export of `columns` under `prefix` into `file`.
    pub(crate) fn scan(prefix: &[u8], columns: Vec<Vec<u8>>, file: Vec<u8>) -> Plan {
        Plan { prefix: prefix.to_vec(), columns, via: None, file, table: None }
    }
}

/// Run `export-csv`; the exit code.
pub(crate) fn run(s: &mut Session, common: &Common) -> u8 {
    let Some(mut plan) = parse(&common.args) else { return 1 };
    if let Some(table) = plan.table.clone() {
        let d = match super::described::fetch(s, super::described::Kind::Table, &table) {
            Ok(Some(d)) => d,
            Ok(None) => {
                eprint_bytes(&[b"kevy-cli: export-csv: no table named '", &table, b"'\n"]);
                return 1;
            }
            Err(()) => return 1,
        };
        plan.prefix = d.text(b"prefix").unwrap_or_default().to_vec();
        if plan.columns.is_empty() {
            plan.columns = d.columns().into_iter().map(|(c, _)| c).collect();
        }
    }
    export(s, &plan)
}

/// Write the export `plan` describes; the exit code.
pub(crate) fn export(s: &mut Session, plan: &Plan) -> u8 {
    let mut out: Box<dyn Write> = if plan.file == b"-" {
        Box::new(std::io::stdout().lock())
    } else {
        match std::fs::File::create(String::from_utf8_lossy(&plan.file).as_ref()) {
            Ok(f) => Box::new(std::io::BufWriter::new(f)),
            Err(e) => {
                let why = crate::rcli::conn::strerror(&e);
                eprint_bytes(&[
                    b"kevy-cli: export-csv: cannot create '",
                    &plan.file,
                    b"': ",
                    why.as_bytes(),
                    b"\n",
                ]);
                return 1;
            }
        }
    };
    let mut header = vec![b"key".to_vec()];
    header.extend(plan.columns.iter().cloned());
    let written = line(&mut out, &header).and_then(|()| match &plan.via {
        Some(query) => via_query(s, plan, query, &mut out),
        None => via_scan(s, plan, &mut out),
    });
    match written.and_then(|n| out.flush().map(|()| n)) {
        Ok(n) => {
            eprint_bytes(&[format!("exported {n} rows\n").as_bytes()]);
            0
        }
        Err(e) => {
            eprint_bytes(&[
                b"kevy-cli: export-csv: ",
                crate::rcli::conn::strerror(&e).as_bytes(),
                b"\n",
            ]);
            2
        }
    }
}

fn line(out: &mut dyn Write, fields: &[Vec<u8>]) -> std::io::Result<()> {
    let quoted: Vec<Vec<u8>> = fields.iter().map(|f| csv::field(f, b',')).collect();
    out.write_all(&quoted.join(&b','))?;
    out.write_all(b"\r\n")
}

fn io_error(text: String) -> std::io::Error {
    std::io::Error::other(text)
}

/// SCAN MATCH prefix*, then HMGET the columns of each page's keys.
fn via_scan(s: &mut Session, plan: &Plan, out: &mut dyn Write) -> std::io::Result<u64> {
    eprint_bytes(&[
        b"kevy-cli: export-csv walks the whole keyspace with SCAN; --via uses an index instead\n",
    ]);
    let pattern = [plan.prefix.as_slice(), b"*"].concat();
    let (mut cursor, mut rows) = (b"0".to_vec(), 0u64);
    loop {
        let page = s
            .request(&[b"SCAN", &cursor, b"MATCH", &pattern, b"COUNT", b"512"])
            .map_err(|e| io_error(e.text()))?;
        let Reply::Array(parts) = page else {
            return Err(io_error("SCAN did not answer with a page".into()));
        };
        let (Some(Reply::Bulk(next)), Some(Reply::Array(keys))) = (parts.first(), parts.get(1))
        else {
            return Err(io_error("SCAN did not answer with a page".into()));
        };
        rows += write_page(s, plan, keys, out)?;
        cursor = next.clone();
        if cursor == b"0" {
            return Ok(rows);
        }
    }
}

/// HMGET the columns of one SCAN page's keys and write their rows.
fn write_page(
    s: &mut Session,
    plan: &Plan,
    keys: &[Reply],
    out: &mut dyn Write,
) -> std::io::Result<u64> {
    let keys: Vec<Vec<u8>> = keys
        .iter()
        .filter_map(|k| if let Reply::Bulk(b) = k { Some(b.clone()) } else { None })
        .collect();
    let columns: Vec<&[u8]> = plan.columns.iter().map(Vec::as_slice).collect();
    let commands: Vec<Vec<&[u8]>> =
        keys.iter().map(|k| [&[&b"HMGET"[..], k.as_slice()][..], &columns].concat()).collect();
    let values = if commands.is_empty() {
        Vec::new()
    } else {
        s.pipeline(&commands).map_err(|e| io_error(e.text()))?
    };
    for (key, reply) in keys.iter().zip(values) {
        let mut fields = vec![key.clone()];
        if let Reply::Array(cells) = reply {
            fields.extend(
                cells.into_iter().map(|c| if let Reply::Bulk(b) = c { b } else { Vec::new() }),
            );
        }
        line(out, &fields)?;
    }
    Ok(keys.len() as u64)
}

/// The query's pages, its FIELDS set to the columns.
fn via_query(
    s: &mut Session,
    plan: &Plan,
    query: &[Vec<u8>],
    out: &mut dyn Write,
) -> std::io::Result<u64> {
    let mut argv = query.to_vec();
    if let Some(at) = argv.iter().position(|w| w.eq_ignore_ascii_case(b"FIELDS")) {
        argv.truncate(at);
    }
    argv.push(b"FIELDS".to_vec());
    argv.extend(plan.columns.iter().cloned());
    let paging = Paging { all: true, max_rows: usize::MAX };
    let collected =
        collect(s, &argv, &paging).ok_or_else(|| io_error("the query failed".into()))?;
    let rows = collected.rows;
    let at = |name: &[u8]| rows.columns.iter().position(|c| c == name);
    for row in &rows.rows {
        let text = |i: Option<usize>| match i.map(|i| &row[i]) {
            Some(Cell::Text(t)) => t.clone(),
            Some(Cell::Int(n)) => n.to_string().into_bytes(),
            _ => Vec::new(),
        };
        let mut fields = vec![text(at(b"key"))];
        fields.extend(plan.columns.iter().map(|c| text(at(c))));
        line(out, &fields)?;
    }
    Ok(rows.rows.len() as u64)
}

fn parse(args: &[Vec<u8>]) -> Option<Plan> {
    let mut plan = Plan::scan(b"", Vec::new(), Vec::new());
    let mut i = 0;
    while i < args.len() {
        match (args[i].as_slice(), args.get(i + 1)) {
            (b"--prefix", Some(v)) => plan.prefix = v.clone(),
            (b"--columns", Some(v)) => {
                plan.columns = v.split(|&b| b == b',').map(<[u8]>::to_vec).collect()
            }
            (b"--via", Some(v)) => match crate::rcli::splitargs::split_args(v) {
                Some(words) => plan.via = Some(words),
                None => {
                    eprint_bytes(&[
                        b"kevy-cli: export-csv: cannot split --via '",
                        v,
                        b"' (unbalanced quotes)\n",
                    ]);
                    return None;
                }
            },
            (b"--table", Some(v)) => plan.table = Some(v.clone()),
            (file, _) if plan.file.is_empty() && !file.starts_with(b"--") => {
                plan.file = file.to_vec();
                i += 1;
                continue;
            }
            _ => {
                eprint_bytes(&[b"usage: kevy-cli export-csv (--prefix p --columns a,b | --table t) [--via \"IDX.QUERY ...\"] <file|->\n"]);
                return None;
            }
        }
        i += 2;
    }
    let ready = !plan.file.is_empty()
        && (plan.table.is_some()
            || (!plan.columns.is_empty() && (!plan.prefix.is_empty() || plan.via.is_some())));
    if !ready {
        eprint_bytes(&[b"usage: kevy-cli export-csv (--prefix p --columns a,b | --table t) [--via \"IDX.QUERY ...\"] <file|->\n"]);
        return None;
    }
    Some(plan)
}

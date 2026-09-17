//! Which relational tool a command names: exact lowercase names only, so
//! every other word — WATCH included — still goes to the server.

use crate::rcli::session::{Connect, Session, eprint_bytes};
use kevy_resp::Reply;

/// The tools, by the name that runs them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tool {
    Tables,
    Indexes,
    Views,
    Describe,
    DescribePlus,
    Query,
    Explain,
    Advise,
    Status,
    WaitReady,
    Watch,
    Run,
    ImportCsv,
    ExportCsv,
    Feed,
}

pub(crate) fn tool_named(name: &[u8]) -> Option<Tool> {
    Some(match name {
        b"tables" => Tool::Tables,
        b"indexes" => Tool::Indexes,
        b"views" => Tool::Views,
        b"describe" => Tool::Describe,
        b"describe+" => Tool::DescribePlus,
        b"query" => Tool::Query,
        b"explain" => Tool::Explain,
        b"advise" => Tool::Advise,
        b"status" => Tool::Status,
        b"wait-ready" => Tool::WaitReady,
        b"watch" => Tool::Watch,
        b"run" => Tool::Run,
        b"import-csv" => Tool::ImportCsv,
        b"export-csv" => Tool::ExportCsv,
        b"feed" => Tool::Feed,
        _ => return None,
    })
}

/// Run the tool `command` names; `None` when it names none.
pub(crate) fn route(s: &mut Session, command: &[Vec<u8>]) -> Option<u8> {
    let (name, rest) = command.split_first()?;
    let tool = tool_named(name)?;
    Some(run_tool(s, tool, rest))
}

/// Run `tool` with `args`, on the session's connection (opened if there is
/// none, so a REPL keeps its MULTI and SELECT state).
pub(crate) fn run_tool(s: &mut Session, tool: Tool, args: &[Vec<u8>]) -> u8 {
    let Some(mut common) = super::options::parse(args, s.opts.output) else { return 1 };
    common.style.expanded |= s.rds.expanded;
    common.timing |= s.rds.timing;
    if s.in_multi {
        // Its commands would be queued, not answered.
        eprint_bytes(&[
            b"kevy-cli: relational commands do not run inside MULTI; EXEC or DISCARD first\n",
        ]);
        return 1;
    }
    if s.conn.is_none() && !s.connect(Connect::Report) {
        return 1;
    }
    let started = std::time::Instant::now();
    let code = dispatch(s, tool, &common);
    if common.timing {
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        crate::rcli::send::write_out(format!("Time: {ms:.3} ms\n").as_bytes());
    }
    code
}

// LOC-WAIVER: a table — one arm per tool.
fn dispatch(s: &mut Session, tool: Tool, common: &super::options::Common) -> u8 {
    match tool {
        Tool::Tables | Tool::Indexes | Tool::Views => super::catalog::list(s, tool, common),
        Tool::Describe | Tool::DescribePlus => {
            super::describe::run(s, tool == Tool::DescribePlus, common)
        }
        Tool::Query => super::query::run(s, common),
        Tool::Explain => super::explain::run(s, common),
        Tool::Advise => super::advise::run(s, common),
        Tool::Status => super::status::run(s, common),
        Tool::WaitReady => super::wait_ready::run(s, common),
        Tool::Watch => super::watch::run(s, common),
        Tool::Run => super::script::run(s, common),
        Tool::ImportCsv => super::import_csv::run(s, common),
        Tool::ExportCsv => super::export_csv::run(s, common),
        Tool::Feed => super::feed::run(s, common),
    }
}

/// Send `argv`; the reply, or `None` after printing the error or the lost
/// link the way the redis-cli half does.
pub(crate) fn ask(s: &mut Session, argv: &[&[u8]]) -> Option<Reply> {
    match s.request(argv) {
        Ok(Reply::Error(msg)) => {
            eprint_bytes(&[b"(error) ", &msg, b"\n"]);
            None
        }
        Ok(reply) => Some(reply),
        Err(e) => {
            eprint_bytes(&[b"Error: ", e.text().as_bytes(), b"\n"]);
            None
        }
    }
}

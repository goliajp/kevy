//! The command-line face of `lint`: arguments, and the report each
//! question prints.

use std::io;
use std::process::ExitCode;

use super::{column_pairs, overlap};
use crate::link::Link;
use kevy_resp_client::Reply;

/// The declared prefix of a table, from `TABLE.LIST`.
fn table_prefix(client: &mut dyn Link, table: &str) -> io::Result<String> {
    let Reply::Array(tables) = client.request_borrowed(&[b"TABLE.LIST"])? else {
        return Err(io::Error::other("TABLE.LIST did not answer with a list"));
    };
    for t in &tables {
        let Reply::Array(items) = t else { continue };
        let f = crate::doctor::fields(items);
        let named = f.iter().any(|(k, v)| k == "name" && v == table);
        if named && let Some((_, p)) = f.iter().find(|(k, _)| k == "prefix") {
            return Ok(p.clone());
        }
    }
    Err(io::Error::other(format!("no declared table named '{table}'")))
}

/// What `lint` takes besides the connection.
struct LintArgs {
    sub: String,
    prefix: String,
    table: String,
    sample: usize,
    threshold: u32,
}

fn parse_lint(args: &[String]) -> Result<LintArgs, String> {
    let mut scan = crate::tools::argscan::Scan::new(args);
    let sub = scan.next().unwrap_or_default().to_string();
    let mut a =
        LintArgs { sub, prefix: String::new(), table: String::new(), sample: 1000, threshold: 90 };
    while let Some(word) = scan.next() {
        match word {
            "--prefix" => a.prefix = scan.value("--prefix")?.to_string(),
            "--sample" => a.sample = scan.number("--sample")?,
            "--threshold" => a.threshold = scan.number("--threshold")?,
            w if !w.starts_with('-') && a.table.is_empty() => a.table = w.to_string(),
            other => return Err(crate::tools::argscan::unexpected(other)),
        }
    }
    Ok(a)
}

/// `lint overlap --prefix <p>` / `lint columns <table> [--sample N]
/// [--threshold PCT]` on `client`.
pub(crate) fn run_on(client: &mut dyn Link, args: &[String]) -> ExitCode {
    let a = match parse_lint(args) {
        Ok(a) => a,
        Err(msg) => return lint_usage(&msg),
    };
    match a.sub.as_str() {
        "overlap" if a.table.is_empty() => run_overlap(client, &a.prefix),
        "overlap" => lint_usage(&format!("unexpected '{}'", a.table)),
        "columns" => run_columns(client, &a.table, a.sample, a.threshold),
        other => lint_usage(&format!("unknown subcommand '{other}'")),
    }
}

fn lint_usage(msg: &str) -> ExitCode {
    eprintln!("kevy-cli lint: {msg}");
    eprintln!("usage: kevy-cli --kevy lint overlap --prefix <p>");
    eprintln!("       kevy-cli --kevy lint columns <table> [--sample N] [--threshold PCT]");
    ExitCode::FAILURE
}

/// Overlap is an answer, not a hint: a column cannot carry a dimension
/// that names more than one owner, so a non-empty intersection exits
/// non-zero and a declaring script stops.
fn run_overlap(client: &mut dyn Link, prefix: &str) -> ExitCode {
    if prefix.is_empty() {
        eprintln!("kevy-cli lint overlap: --prefix names the family of owner keys");
        return ExitCode::FAILURE;
    }
    let o = match overlap(client, prefix) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("kevy-cli lint overlap: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("{} owner(s) under {prefix}, {} distinct name(s)", o.owners, o.names);
    if o.skipped > 0 {
        println!("  ({} key(s) under this prefix are not collections and were skipped)", o.skipped);
    }
    if o.owners == 0 {
        println!("no collection under {prefix} — is that the right prefix?");
        return ExitCode::FAILURE;
    }
    if o.shared == 0 {
        println!("no name appears under more than one owner — a column can carry this dimension");
        return ExitCode::SUCCESS;
    }
    println!("{} name(s) appear under more than one owner:", o.shared);
    for (name, owners) in &o.examples {
        println!("  {name}  →  {}", owners.join(", "));
    }
    println!(
        "this dimension is multi-valued, so no column can hold it — model a membership row \
         per (owner, item) and let an ORDERPATH sort it"
    );
    ExitCode::FAILURE
}

/// Coincidence is a suspicion — two columns may legitimately agree —
/// so this reports and exits zero whatever it finds.
fn run_columns(client: &mut dyn Link, table: &str, sample: usize, threshold: u32) -> ExitCode {
    if table.is_empty() {
        eprintln!("kevy-cli lint columns: name a declared table");
        return ExitCode::FAILURE;
    }
    let prefix = match table_prefix(client, table) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("kevy-cli lint columns: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (rows, found) = match column_pairs(client, &prefix, sample, threshold) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("kevy-cli lint columns: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("{table}: {rows} row(s) sampled under {prefix}");
    if found.is_empty() {
        println!("no two columns agree on {threshold}% or more of them");
        return ExitCode::SUCCESS;
    }
    for c in &found {
        println!("  {} and {} agree on {}% ({}/{})", c.a, c.b, c.percent(), c.same, c.compared);
    }
    println!(
        "a column copied to get a second sort order is the shape lesson 6 warns about — \
         the answer is another ORDERPATH; ask IDX.ADVISE which one"
    );
    ExitCode::SUCCESS
}

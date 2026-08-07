//! `kevy-cli sql` — the kevy-sql declaration compiler as a subcommand.
//!
//! Two shapes over the same file. `sql compile` is build-time: it
//! produces commands, so one unservable view is an error. `sql plan` is
//! migration day: it reports what becomes of **every** query, because
//! "34 of your 40 work, here is what the other 6 need" is the answer
//! someone arriving with a schema is actually looking for.
//!
//! `sql compile <file.sql>` prints the compiled script;
//! `sql compile <file.sql> --apply --url <host:port>` additionally runs
//! the declaration commands against a server, printing each reply, and
//! exits non-zero on any error reply. Query cards are runtime
//! templates — they are printed, never applied.

use kevy_cli::{Reply, format_reply};
use kevy_resp_client::RespClient;
use std::process::ExitCode;

#[derive(PartialEq, Eq)]
enum Sub {
    Compile,
    Plan,
}

struct SqlArgs {
    sub: Sub,
    file: String,
    apply: bool,
    host: String,
    port: u16,
}

fn parse_sql_args(args: &[String]) -> Result<SqlArgs, String> {
    let mut it = args.iter();
    let sub = match it.next().map(String::as_str) {
        Some("compile") => Sub::Compile,
        Some("plan") => Sub::Plan,
        Some(other) => return Err(format!("unknown sql subcommand '{other}'")),
        None => return Err("missing subcommand".into()),
    };
    let mut out = SqlArgs {
        sub,
        file: String::new(),
        apply: false,
        host: crate::DEFAULT_HOST.to_string(),
        port: crate::DEFAULT_PORT,
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--apply" => out.apply = true,
            "--url" => {
                let Some(url) = it.next() else { return Err("--url requires host:port".into()) };
                let Some((h, p)) = url.rsplit_once(':') else {
                    return Err(format!("--url '{url}' must be host:port"));
                };
                out.host = h.to_string();
                out.port = p.parse().map_err(|_| format!("--url port '{p}' is not a port"))?;
            }
            other if other.starts_with('-') => return Err(format!("unknown flag {other}")),
            other if out.file.is_empty() => out.file = other.to_string(),
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    if out.file.is_empty() {
        return Err("missing <file.sql>".into());
    }
    if out.apply && out.sub == Sub::Plan {
        return Err("plan never applies anything — it reads the file and reports".into());
    }
    Ok(out)
}

/// Entry: `kevy-cli sql …` (args exclude the leading `sql`).
pub(crate) fn run_sql_cli(args: &[String]) -> ExitCode {
    let a = match parse_sql_args(args) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("kevy-cli sql: {msg}");
            eprintln!("usage: kevy-cli sql compile <file.sql> [--apply --url <host:port>]");
            eprintln!("       kevy-cli sql plan <file.sql>");
            return ExitCode::FAILURE;
        }
    };
    let src = match std::fs::read_to_string(&a.file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kevy-cli sql: {}: {e}", a.file);
            return ExitCode::FAILURE;
        }
    };
    if a.sub == Sub::Plan {
        return run_plan(&a.file, &src);
    }
    let comp = match kevy_sql::compile(&src) {
        Ok(c) => c,
        Err(e) => {
            // The compiler's errors are the product: file:line, named,
            // teaching. Print them exactly.
            eprintln!("kevy-cli sql: {}: {e}", a.file);
            return ExitCode::FAILURE;
        }
    };
    if !a.apply {
        print!("{}", comp.render_script());
        return ExitCode::SUCCESS;
    }
    apply(&a, &comp)
}

/// Run the declaration commands in order; stop (and exit non-zero) on
/// the first error reply — later declarations depend on earlier ones.
fn apply(a: &SqlArgs, comp: &kevy_sql::Compilation) -> ExitCode {
    let mut conn = match RespClient::connect(&a.host, a.port) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kevy-cli sql: could not connect to {}:{}: {e}", a.host, a.port);
            return ExitCode::FAILURE;
        }
    };
    for cmd in &comp.commands {
        let argv: Vec<Vec<u8>> = cmd.iter().map(|s| s.clone().into_bytes()).collect();
        let reply = match conn.request(&argv) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("kevy-cli sql: {e}");
                return ExitCode::FAILURE;
            }
        };
        println!("{} {} \u{2192} {}", cmd[0], cmd[1], format_reply(&reply, 0));
        if matches!(reply, Reply::Error(_) | Reply::BlobError(_)) {
            eprintln!("kevy-cli sql: apply stopped at the error above (declarations are ordered)");
            return ExitCode::FAILURE;
        }
    }
    if !comp.query_cards.is_empty() {
        println!(
            "{} query card(s) are runtime templates \u{2014} not applied; see `kevy-cli sql compile {}`",
            comp.query_cards.len(),
            a.file
        );
    }
    ExitCode::SUCCESS
}

/// `sql plan <file.sql>` — every query's fate, then the count.
///
/// Exits non-zero when any query is unserved. Unlike a `doctor`
/// warning, this is not information: a query with no declared path
/// cannot run at all, so it blocks the move until the schema changes.
fn run_plan(file: &str, src: &str) -> ExitCode {
    let plan = match kevy_sql::plan(src) {
        Ok(p) => p,
        Err(e) => {
            // A schema that does not parse has no plan — that failure
            // stays an error, and keeps its file:line.
            eprintln!("kevy-cli sql plan: {file}: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("{} table(s) to declare:", plan.declares.len());
    for d in &plan.declares {
        println!("  {}", d[1]);
    }
    let served = plan.queries.len() - plan.unserved();
    println!("\n{} quer(ies) — {} served, {} not", plan.queries.len(), served, plan.unserved());
    print_entries(&plan);
    for n in &plan.notes {
        println!("note: {n}");
    }
    if plan.unserved() == 0 {
        println!("\nplan: every query is served by a declared path");
        return ExitCode::SUCCESS;
    }
    println!(
        "\nplan: {} of {} quer(ies) need a declaration change before this schema moves",
        plan.unserved(),
        plan.queries.len()
    );
    ExitCode::FAILURE
}

/// Served queries name the paths they ride; unserved ones carry the
/// compiler's own refusal, which already teaches the fix.
fn print_entries(plan: &kevy_sql::Plan) {
    let (served, unserved): (Vec<_>, Vec<_>) =
        plan.queries.iter().partition(|q| q.served.is_served());
    if !served.is_empty() {
        println!("\n  served:");
        for q in served {
            let kevy_sql::Served::Yes { paths, .. } = &q.served else { continue };
            println!("    {:<24} {}", q.name, paths.join(" + "));
        }
    }
    if !unserved.is_empty() {
        println!("\n  not served:");
        for q in unserved {
            let kevy_sql::Served::No { reason } = &q.served else { continue };
            println!("    line {:<5} {}", q.line, q.name);
            println!("      {reason}");
        }
    }
}

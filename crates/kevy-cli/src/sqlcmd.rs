//! `kevy-cli sql` — the kevy-sql declaration compiler as a subcommand.
//!
//! `sql compile <file.sql>` prints the compiled script;
//! `sql compile <file.sql> --apply --url <host:port>` additionally runs
//! the declaration commands against a server, printing each reply, and
//! exits non-zero on any error reply. Query cards are runtime
//! templates — they are printed, never applied.

use kevy_cli::{Reply, format_reply};
use kevy_resp_client::RespClient;
use std::process::ExitCode;

struct SqlArgs {
    file: String,
    apply: bool,
    host: String,
    port: u16,
}

fn parse_sql_args(args: &[String]) -> Result<SqlArgs, String> {
    let mut it = args.iter();
    match it.next().map(String::as_str) {
        Some("compile") => {}
        Some(other) => return Err(format!("unknown sql subcommand '{other}'")),
        None => return Err("missing subcommand".into()),
    }
    let mut out = SqlArgs {
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
    Ok(out)
}

/// Entry: `kevy-cli sql …` (args exclude the leading `sql`).
pub(crate) fn run_sql_cli(args: &[String]) -> ExitCode {
    let a = match parse_sql_args(args) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("kevy-cli sql: {msg}");
            eprintln!("usage: kevy-cli sql compile <file.sql> [--apply --url <host:port>]");
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

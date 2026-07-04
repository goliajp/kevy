//! kevy-cli — a small redis-cli-style client for [kevy] or any RESP server.
//!
//! Pure Rust, zero third-party dependencies (just [kevy-resp] + `std`).
//!
//! ```text
//! kevy-cli [-h host] [-p port] [command args...]
//! ```
//!
//! With a trailing command it runs once and exits; otherwise it starts an
//! interactive REPL.
//!
//! The protocol pieces (`RespClient`, `format_reply`) live in the sibling
//! `kevy_cli` library so other tools / tests / scripts can reuse them
//! without depending on the binary.
//!
//! [kevy]: https://crates.io/crates/kevy
//! [kevy-resp]: https://crates.io/crates/kevy-resp
#![forbid(unsafe_code)]

use kevy_cli::{Reply, format_reply};
use kevy_resp_client::RespClient;
use std::io::{self, BufRead, Write};
use std::process::ExitCode;

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 6379;

fn main() -> ExitCode {
    // --help / --version short-circuit BEFORE we touch TCP, so the binary
    // works in healthchecks / image-smoke / `--help` exploration without a
    // running server. `-h` keeps its redis-cli meaning (host); only the long
    // `--help` and `-V` / `--version` short-circuit. Mirrors the kevy server
    // pattern shipped in v1.0.4.
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--help" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            "--version" | "-V" => {
                println!("kevy-cli {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            _ => {}
        }
    }

    // v1.40 — `backup` / `restore` subcommands. Routed BEFORE the RESP
    // client setup because they're file-only operations (no TCP).
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() && args[0] == "backup" {
        return run_backup_cli(&args[1..]);
    }
    if !args.is_empty() && args[0] == "restore" {
        return run_restore_cli(&args[1..]);
    }
    // v2.10 — migration subcommands (TCP, host/port flags inline).
    if !args.is_empty() && (args[0] == "export" || args[0] == "import") {
        return run_migrate_cli(&args);
    }
    if !args.is_empty()
        && matches!(args[0].as_str(), "copy-prefix" | "delete-prefix" | "digest" | "diff" | "inspect")
    {
        return run_bulk_cli(&args);
    }
    // Strip subcommand arg if it was something other than a flag we
    // already handled, to preserve the existing redis-cli arg shape.
    let _ = &mut args;

    let cfg = Config::from_args(std::env::args().skip(1));
    let mut conn = match RespClient::connect(&cfg.host, cfg.port) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "kevy-cli: could not connect to {}:{}: {e}",
                cfg.host, cfg.port
            );
            return ExitCode::FAILURE;
        }
    };
    if cfg.command.is_empty() {
        repl(&mut conn, &cfg)
    } else {
        run_once(&mut conn, &cfg.command)
    }
}

fn print_help() {
    let v = env!("CARGO_PKG_VERSION");
    println!(
        "\
kevy-cli {v} — redis-cli-style REPL for kevy or any RESP server.

USAGE:
    kevy-cli [-h <host>] [-p <port>] [command [args ...]]

OPTIONS:
    -h <host>           Server hostname (default: 127.0.0.1)
    -p <port>           Server port (default: 6379)
    --help              Show this help and exit
    -V, --version       Print version and exit

With a trailing command, runs once and exits non-zero on a RESP error.
Without a command, opens an interactive REPL (Ctrl-D / `quit` / `exit` to leave).

EXAMPLES:
    kevy-cli                            # REPL against 127.0.0.1:6379
    kevy-cli -p 6004                    # REPL against kevy default port
    kevy-cli -h prod.internal ping      # one-shot PING
    kevy-cli -p 6004 set greet hello    # one-shot SET, exits 0

Docs: https://github.com/goliajp/kevy"
    );
}

/// Parsed command-line configuration.
struct Config {
    host: String,
    port: u16,
    command: Vec<Vec<u8>>,
}

impl Config {
    fn from_args(args: impl Iterator<Item = String>) -> Config {
        let mut host = DEFAULT_HOST.to_string();
        let mut port = DEFAULT_PORT;
        let mut command = Vec::new();
        let mut args = args.peekable();
        // Leading -h/-p flags, then everything else is the command.
        while let Some(arg) = args.peek() {
            match arg.as_str() {
                "-h" => {
                    args.next();
                    if let Some(h) = args.next() {
                        host = h;
                    }
                }
                "-p" => {
                    args.next();
                    if let Some(p) = args.next().and_then(|s| s.parse().ok()) {
                        port = p;
                    }
                }
                _ => break,
            }
        }
        command.extend(args.map(String::into_bytes));
        Config {
            host,
            port,
            command,
        }
    }
}

/// Run a single command, print its reply, exit non-zero on a RESP error.
fn run_once(conn: &mut RespClient, command: &[Vec<u8>]) -> ExitCode {
    match conn.request(command) {
        Ok(reply) => {
            println!("{}", format_reply(&reply, 0));
            if matches!(reply, Reply::Error(_)) {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("kevy-cli: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Interactive read-eval-print loop.
fn repl(conn: &mut RespClient, cfg: &Config) -> ExitCode {
    let prompt = format!("{}:{}> ", cfg.host, cfg.port);
    let stdin = io::stdin();
    let mut line = String::new();
    loop {
        print!("{prompt}");
        let _ = io::stdout().flush();
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => return ExitCode::SUCCESS, // EOF (Ctrl-D)
            Ok(_) => {}
            Err(e) => {
                eprintln!("kevy-cli: {e}");
                return ExitCode::FAILURE;
            }
        }
        let args = split_args(line.trim_end());
        if args.is_empty() {
            continue;
        }
        if let [only] = args.as_slice()
            && (only.eq_ignore_ascii_case(b"quit") || only.eq_ignore_ascii_case(b"exit"))
        {
            return ExitCode::SUCCESS;
        }
        match conn.request(&args) {
            Ok(reply) => println!("{}", format_reply(&reply, 0)),
            Err(e) => {
                eprintln!("kevy-cli: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
}

/// Split a line into arguments on ASCII whitespace (no quote handling yet).
fn split_args(line: &str) -> Vec<Vec<u8>> {
    line.split_whitespace()
        .map(|s| s.as_bytes().to_vec())
        .collect()
}

// ───────────── v1.40 backup / restore ─────────────

fn run_backup_cli(args: &[String]) -> ExitCode {
    let (data_dir, out_path) = match parse_backup_args(args) {
        Ok(t) => t,
        Err(msg) => {
            eprintln!("kevy-cli backup: {msg}");
            eprintln!("usage: kevy-cli backup --data-dir <path> --to <out.kevybkp>");
            return ExitCode::FAILURE;
        }
    };
    match kevy_cli::backup::run_backup(data_dir, out_path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kevy-cli backup failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_restore_cli(args: &[String]) -> ExitCode {
    let (in_path, target_dir) = match parse_restore_args(args) {
        Ok(t) => t,
        Err(msg) => {
            eprintln!("kevy-cli restore: {msg}");
            eprintln!("usage: kevy-cli restore --from <in.kevybkp> --to <data_dir>");
            return ExitCode::FAILURE;
        }
    };
    match kevy_cli::backup::run_restore(in_path, target_dir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kevy-cli restore failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse_backup_args(args: &[String]) -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    let mut data_dir = None;
    let mut out_path = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--data-dir" => {
                i += 1;
                data_dir = Some(std::path::PathBuf::from(
                    args.get(i).ok_or_else(|| "--data-dir requires a value".to_string())?,
                ));
            }
            "--to" => {
                i += 1;
                out_path = Some(std::path::PathBuf::from(
                    args.get(i).ok_or_else(|| "--to requires a value".to_string())?,
                ));
            }
            other => return Err(format!("unknown flag {other}")),
        }
        i += 1;
    }
    Ok((
        data_dir.ok_or_else(|| "--data-dir missing".to_string())?,
        out_path.ok_or_else(|| "--to missing".to_string())?,
    ))
}

fn parse_restore_args(args: &[String]) -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    let mut in_path = None;
    let mut target_dir = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--from" => {
                i += 1;
                in_path = Some(std::path::PathBuf::from(
                    args.get(i).ok_or_else(|| "--from requires a value".to_string())?,
                ));
            }
            "--to" => {
                i += 1;
                target_dir = Some(std::path::PathBuf::from(
                    args.get(i).ok_or_else(|| "--to requires a value".to_string())?,
                ));
            }
            other => return Err(format!("unknown flag {other}")),
        }
        i += 1;
    }
    Ok((
        in_path.ok_or_else(|| "--from missing".to_string())?,
        target_dir.ok_or_else(|| "--to missing".to_string())?,
    ))
}

/// `export [-h host] [-p port] [--prefix p] <out-file>` /
/// `import [-h host] [-p port] [--resume] [--strict] <file>`.
fn run_migrate_cli(args: &[String]) -> ExitCode {
    let verb = args[0].as_str();
    let (mut host, mut port) = (DEFAULT_HOST.to_string(), DEFAULT_PORT);
    let mut prefix: Option<Vec<u8>> = None;
    let (mut resume, mut strict) = (false, false);
    let mut file: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-h" if i + 1 < args.len() => {
                host = args[i + 1].clone();
                i += 2;
            }
            "-p" if i + 1 < args.len() => {
                port = args[i + 1].parse().unwrap_or(DEFAULT_PORT);
                i += 2;
            }
            "--prefix" if i + 1 < args.len() => {
                prefix = Some(args[i + 1].clone().into_bytes());
                i += 2;
            }
            "--resume" => {
                resume = true;
                i += 1;
            }
            "--strict" => {
                strict = true;
                i += 1;
            }
            other => {
                file = Some(other.to_string());
                i += 1;
            }
        }
    }
    let Some(file) = file else {
        eprintln!("usage: kevy-cli {verb} [-h host] [-p port] [--prefix p | --resume --strict] <file>");
        return ExitCode::FAILURE;
    };
    let mut client = match kevy_resp_client::RespClient::connect(&host, port) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kevy-cli: connect {host}:{port}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let res = if verb == "export" {
        kevy_cli::migrate::run_export(&mut client, prefix.as_deref(), std::path::Path::new(&file))
            .map(|n| println!("exported {n} keys -> {file}"))
    } else {
        kevy_cli::migrate::run_import(&mut client, std::path::Path::new(&file), resume, strict)
            .map(|r| {
                println!(
                    "imported: {} ok, {} errors, offset {}",
                    r.sent, r.errors, r.offset
                )
            })
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kevy-cli {verb}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// v2.10 bulk/diagnostic subcommands. Shapes:
/// `copy-prefix [-h host -p port] [--rate N] <src-prefix> <dst-prefix>`
/// `delete-prefix [-h host -p port] [--rate N] [--dry-run] <prefix>`
/// `digest [-h host -p port] <prefix>`
/// `diff <hostA:portA> <hostB:portB> <prefix…>`
/// `inspect [-h host -p port] <prefix>`
fn run_bulk_cli(args: &[String]) -> ExitCode {
    let verb = args[0].as_str();
    let (mut host, mut port) = (DEFAULT_HOST.to_string(), DEFAULT_PORT);
    let mut rate = 0u64;
    let mut dry_run = false;
    let mut pos: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-h" if i + 1 < args.len() => {
                host = args[i + 1].clone();
                i += 2;
            }
            "-p" if i + 1 < args.len() => {
                port = args[i + 1].parse().unwrap_or(DEFAULT_PORT);
                i += 2;
            }
            "--rate" if i + 1 < args.len() => {
                rate = args[i + 1].parse().unwrap_or(0);
                i += 2;
            }
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            other => {
                pos.push(other.to_string());
                i += 1;
            }
        }
    }
    let connect = |host: &str, port: u16| kevy_resp_client::RespClient::connect(host, port);
    let fail = |e: io::Error| {
        eprintln!("kevy-cli {verb}: {e}");
        ExitCode::FAILURE
    };
    match verb {
        "diff" => {
            if pos.len() < 3 {
                eprintln!("usage: kevy-cli diff <hostA:portA> <hostB:portB> <prefix…>");
                return ExitCode::FAILURE;
            }
            let parse = |hp: &str| -> Option<(String, u16)> {
                let (h, p) = hp.rsplit_once(':')?;
                Some((h.to_string(), p.parse().ok()?))
            };
            let (Some((ha, pa)), Some((hb, pb))) = (parse(&pos[0]), parse(&pos[1])) else {
                eprintln!("kevy-cli diff: endpoints must be host:port");
                return ExitCode::FAILURE;
            };
            let (mut ca, mut cb) = match (connect(&ha, pa), connect(&hb, pb)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(e), _) | (_, Err(e)) => return fail(e),
            };
            let prefixes: Vec<Vec<u8>> = pos[2..].iter().map(|p| p.clone().into_bytes()).collect();
            match kevy_cli::bulk::run_diff(&mut ca, &mut cb, &prefixes, &mut io::stdout()) {
                Ok(bad) if bad.is_empty() => ExitCode::SUCCESS,
                Ok(_) => ExitCode::FAILURE,
                Err(e) => fail(e),
            }
        }
        _ => {
            let mut client = match connect(&host, port) {
                Ok(c) => c,
                Err(e) => return fail(e),
            };
            let res: io::Result<()> = match (verb, pos.as_slice()) {
                ("copy-prefix", [src, dst]) => {
                    kevy_cli::bulk::run_copy_prefix(&mut client, src.as_bytes(), dst.as_bytes(), rate)
                        .map(|n| println!("copied {n} keys"))
                }
                ("delete-prefix", [p]) => {
                    kevy_cli::bulk::run_delete_prefix(&mut client, p.as_bytes(), rate, dry_run)
                        .map(|n| println!("{}{n} keys", if dry_run { "would delete " } else { "deleted " }))
                }
                ("digest", [p]) => kevy_cli::bulk::run_digest(&mut client, p.as_bytes())
                    .map(|(n, d)| println!("{n} keys {d}")),
                ("inspect", [p]) => {
                    kevy_cli::bulk::run_inspect(&mut client, p.as_bytes(), &mut io::stdout())
                }
                _ => {
                    eprintln!("kevy-cli {verb}: wrong arguments");
                    return ExitCode::FAILURE;
                }
            };
            match res {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => fail(e),
            }
        }
    }
}


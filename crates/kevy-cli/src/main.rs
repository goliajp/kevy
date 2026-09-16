//! kevy-cli — redis-cli for [kevy] or any RESP server, plus kevy's tools.
//!
//! Pure Rust, zero third-party dependencies (just [kevy-resp] + `std`).
//!
//! ```text
//! kevy-cli [options] [command args...]     # what redis-cli does
//! kevy-cli <tool> [tool options]           # sql, export, import, doctor, …
//! ```
//!
//! The redis-cli half lives in the `kevy_cli::rcli` library module; its
//! acceptance is a byte-for-byte comparison with redis-cli (`bench/cligate.py`).
//!
//! [kevy]: https://crates.io/crates/kevy
//! [kevy-resp]: https://crates.io/crates/kevy-resp
#![forbid(unsafe_code)]

use std::io;
use std::process::ExitCode;

use kevy_cli::{DEFAULT_HOST, DEFAULT_PORT};

mod embed;
mod sql_probe;
mod sqlcmd;

fn main() -> ExitCode {
    use std::os::unix::ffi::OsStringExt;
    let raw: Vec<Vec<u8>> = std::env::args_os().skip(1).map(OsStringExt::into_vec).collect();
    let args: Vec<String> = raw.iter().map(|a| String::from_utf8_lossy(a).into_owned()).collect();
    if let Some(code) = route_subcommand(&args) {
        return code;
    }
    ExitCode::from(kevy_cli::rcli::run(&raw))
}

/// Route the non-REPL subcommands. `Some(code)` = handled, exit with it;
/// `None` = no subcommand matched, continue down the redis-cli path.
/// Say what a rebuild-frame walk left behind. Both `export` and
/// `copy-prefix` read through the same rebuild set, so both can drop a
/// type the set does not cover — and a tool that drops data while
/// printing a success line is the failure this exists to prevent.
fn report_skipped(verb: &str, skipped: &std::collections::BTreeMap<Vec<u8>, u64>) {
    for (ty, count) in skipped {
        eprintln!(
            "kevy-cli {verb}: SKIPPED {count} key(s) of type '{}' — nothing here rebuilds \
             that type, so they were NOT carried",
            String::from_utf8_lossy(ty)
        );
    }
}

fn route_subcommand(args: &[String]) -> Option<ExitCode> {
    // `backup` / `restore` subcommands. Routed BEFORE the RESP
    // client setup because they're file-only operations (no TCP).
    if !args.is_empty() && args[0] == "backup" {
        return Some(run_backup_cli(&args[1..]));
    }
    if !args.is_empty() && args[0] == "restore" {
        return Some(run_restore_cli(&args[1..]));
    }
    // `--embed <dir>`: read-only point-in-time view of an
    // embedded store's data directory. No server, no downtime.
    if !args.is_empty() && args[0] == "--embed" {
        let Some(dir) = args.get(1).cloned() else {
            eprintln!("kevy-cli: --embed needs a data directory (kevy-cli --embed /data/kevy)");
            return Some(ExitCode::FAILURE);
        };
        let cmd: Vec<Vec<u8>> = args[2..].iter().map(|s| s.clone().into_bytes()).collect();
        return Some(embed::run_embed_cli(&dir, &cmd));
    }
    // `sql compile <file.sql> [--apply --url h:p]`: the declaration-time
    // SQL compiler (kevy-sql). File-first; TCP only under --apply.
    if !args.is_empty() && args[0] == "sql" {
        return Some(sqlcmd::run_sql_cli(&args[1..]));
    }
    // The migration-playbook tools: doctor, shadow, lint, backfill-keys.
    // They route together because they are one family — read, report,
    // move nothing — and the dispatch lives next to them.
    if let Some(code) = kevy_cli::route_tool(args) {
        return Some(code);
    }
    // Migration subcommands (TCP, host/port flags inline).
    if !args.is_empty() && (args[0] == "export" || args[0] == "import") {
        return Some(run_migrate_cli(args));
    }
    if !args.is_empty()
        && matches!(
            args[0].as_str(),
            "copy-prefix" | "delete-prefix" | "digest" | "diff" | "inspect"
        )
    {
        return Some(run_bulk_cli(args));
    }
    None
}

/// Run a single command, print its reply, exit non-zero on a RESP error.
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

/// Parse the shared `export` / `import` flags. Returns
/// `(host, port, prefix, resume, strict, file)`; the trailing
/// positional arg (if any) lands in `file`.
fn parse_migrate_flags(
    args: &[String],
) -> (String, u16, Option<Vec<u8>>, bool, bool, Option<String>) {
    let (mut host, mut port) = (DEFAULT_HOST.to_string(), DEFAULT_PORT);
    let mut prefix: Option<Vec<u8>> = None;
    let (mut resume, mut strict) = (false, false);
    let mut file: Option<String> = None;
    let mut i = 0;
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
    (host, port, prefix, resume, strict, file)
}

/// `export [-h host] [-p port] [--prefix p] <out-file>` /
/// `import [-h host] [-p port] [--resume] [--strict] <file>`.
fn run_migrate_cli(args: &[String]) -> ExitCode {
    let verb = args[0].as_str();
    let (host, port, prefix, resume, strict, file) = parse_migrate_flags(&args[1..]);
    let Some(file) = file else {
        eprintln!(
            "usage: kevy-cli {verb} [-h host] [-p port] [--prefix p | --resume --strict] <file>"
        );
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
            .map(|e| {
                report_skipped("export", &e.skipped);
                println!("exported {} keys -> {file}", e.keys);
            })
    } else {
        kevy_cli::migrate::run_import(&mut client, std::path::Path::new(&file), resume, strict).map(
            |r| {
                if resume && r.sent == 0 && r.errors == 0 {
                    // A no-op resume reads as a silent failure without
                    // this — say plainly that the file was already in.
                    println!("imported: already complete (offset {}), nothing to resume", r.offset)
                } else {
                    println!("imported: {} ok, {} errors, offset {}", r.sent, r.errors, r.offset)
                }
            },
        )
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kevy-cli {verb}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Parse the shared bulk-subcommand flags. Returns
/// `(host, port, rate, dry_run, positional_args)`.
fn parse_bulk_flags(args: &[String]) -> (String, u16, u64, bool, Vec<String>) {
    let (mut host, mut port) = (DEFAULT_HOST.to_string(), DEFAULT_PORT);
    let mut rate = 0u64;
    let mut dry_run = false;
    let mut pos: Vec<String> = Vec::new();
    let mut i = 0;
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
    (host, port, rate, dry_run, pos)
}

/// The `diff` arm of [`run_bulk_cli`]:
/// `diff <hostA:portA> <hostB:portB> <prefix…>`.
fn run_diff_cli(pos: &[String]) -> ExitCode {
    let fail = |e: io::Error| {
        eprintln!("kevy-cli diff: {e}");
        ExitCode::FAILURE
    };
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
    let connect = |host: &str, port: u16| kevy_resp_client::RespClient::connect(host, port);
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

/// Bulk/diagnostic subcommands. Shapes:
/// `copy-prefix [-h host -p port] [--rate N] <src-prefix> <dst-prefix>`
/// `delete-prefix [-h host -p port] [--rate N] [--dry-run] <prefix>`
/// `digest [-h host -p port] <prefix>`
/// `diff <hostA:portA> <hostB:portB> <prefix…>`
/// `inspect [-h host -p port] <prefix>`
// LOC-WAIVER: pure subcommand dispatch — eleven arms, each one call.
// Splitting it would put half the table behind a name that means
// nothing but "the other half".
fn run_bulk_cli(args: &[String]) -> ExitCode {
    let verb = args[0].as_str();
    let (host, port, rate, dry_run, pos) = parse_bulk_flags(&args[1..]);
    let fail = |e: io::Error| {
        eprintln!("kevy-cli {verb}: {e}");
        ExitCode::FAILURE
    };
    match verb {
        "diff" => run_diff_cli(&pos),
        _ => {
            let mut client = match kevy_resp_client::RespClient::connect(&host, port) {
                Ok(c) => c,
                Err(e) => return fail(e),
            };
            let res: io::Result<()> = match (verb, pos.as_slice()) {
                ("copy-prefix", [src, dst]) => kevy_cli::bulk::run_copy_prefix(
                    &mut client,
                    src.as_bytes(),
                    dst.as_bytes(),
                    rate,
                )
                .map(|e| {
                    report_skipped("copy-prefix", &e.skipped);
                    println!("copied {} keys", e.keys);
                }),
                ("delete-prefix", [p]) => kevy_cli::bulk::run_delete_prefix(
                    &mut client,
                    p.as_bytes(),
                    rate,
                    dry_run,
                )
                .map(|n| {
                    println!("{}{n} keys", if dry_run { "would delete " } else { "deleted " })
                }),
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

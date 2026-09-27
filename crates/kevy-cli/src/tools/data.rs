//! `backup`/`restore` (a data directory, offline) and `export`/`import`
//! (the keyspace as a RESP stream, online).

use super::argscan::{Scan, unexpected};
use crate::link::Link;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn fail(tool: &str, msg: &str, usage: &str) -> ExitCode {
    eprintln!("kevy-cli {tool}: {msg}");
    eprintln!("usage: kevy-cli --kevy {tool} {usage}");
    ExitCode::FAILURE
}

/// Two required path flags, in any order.
fn two_paths(args: &[String], a: &str, b: &str) -> Result<(PathBuf, PathBuf), String> {
    let (mut first, mut second) = (None, None);
    let mut scan = Scan::new(args);
    while let Some(word) = scan.next() {
        match word {
            w if w == a => first = Some(PathBuf::from(scan.value(a)?)),
            w if w == b => second = Some(PathBuf::from(scan.value(b)?)),
            other => return Err(unexpected(other)),
        }
    }
    Ok((first.ok_or(format!("{a} missing"))?, second.ok_or(format!("{b} missing"))?))
}

/// `backup --data-dir <path> --to <out.kevybkp>`.
pub(crate) fn backup(args: &[String]) -> ExitCode {
    const USAGE: &str = "--data-dir <path> --to <out.kevybkp>";
    let (dir, out) = match two_paths(args, "--data-dir", "--to") {
        Ok(p) => p,
        Err(msg) => return fail("backup", &msg, USAGE),
    };
    report("backup", crate::backup::run_backup(dir, out))
}

/// `restore --from <in.kevybkp> --to <data_dir>`.
pub(crate) fn restore(args: &[String]) -> ExitCode {
    const USAGE: &str = "--from <in.kevybkp> --to <data_dir>";
    let (from, to) = match two_paths(args, "--from", "--to") {
        Ok(p) => p,
        Err(msg) => return fail("restore", &msg, USAGE),
    };
    report("restore", crate::backup::run_restore(from, to))
}

fn report(tool: &str, done: std::io::Result<()>) -> ExitCode {
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kevy-cli {tool} failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// What `export`/`import` take besides the connection.
struct Stream {
    prefix: Option<Vec<u8>>,
    resume: bool,
    strict: bool,
    file: String,
}

fn stream_args(args: &[String], export: bool) -> Result<Stream, String> {
    let mut s = Stream { prefix: None, resume: false, strict: false, file: String::new() };
    let mut scan = Scan::new(args);
    while let Some(word) = scan.next() {
        match word {
            "--prefix" if export => s.prefix = Some(scan.value("--prefix")?.as_bytes().to_vec()),
            "--resume" if !export => s.resume = true,
            "--strict" if !export => s.strict = true,
            w if !w.starts_with('-') && s.file.is_empty() => s.file = w.to_string(),
            other => return Err(unexpected(other)),
        }
    }
    if s.file.is_empty() {
        return Err("give the file".into());
    }
    Ok(s)
}

/// `export [--prefix p] <out-file>`.
pub(crate) fn export(link: &mut dyn Link, args: &[String]) -> ExitCode {
    let s = match stream_args(args, true) {
        Ok(s) => s,
        Err(msg) => return fail("export", &msg, "[--prefix p] <file>"),
    };
    let done = crate::migrate::run_export(link, s.prefix.as_deref(), Path::new(&s.file)).map(|e| {
        report_skipped("export", &e.skipped);
        println!("exported {} keys -> {}", e.keys, s.file);
    });
    report("export", done)
}

/// `import [--resume] [--strict] <file>`.
pub(crate) fn import(link: &mut dyn Link, args: &[String]) -> ExitCode {
    let s = match stream_args(args, false) {
        Ok(s) => s,
        Err(msg) => return fail("import", &msg, "[--resume] [--strict] <file>"),
    };
    let done = crate::migrate::run_import(link, Path::new(&s.file), s.resume, s.strict).map(|r| {
        if s.resume && r.sent == 0 && r.errors == 0 {
            // A no-op resume reads as a silent failure without this.
            println!("imported: already complete (offset {}), nothing to resume", r.offset)
        } else {
            println!("imported: {} ok, {} errors, offset {}", r.sent, r.errors, r.offset)
        }
    });
    report("import", done)
}

/// Say what a rebuild-frame walk left behind: a tool that drops data
/// while printing a success line is the failure this exists to prevent.
pub(crate) fn report_skipped(verb: &str, skipped: &std::collections::BTreeMap<Vec<u8>, u64>) {
    for (ty, count) in skipped {
        eprintln!(
            "kevy-cli {verb}: SKIPPED {count} key(s) of type '{}' — nothing here rebuilds \
             that type, so they were NOT carried",
            String::from_utf8_lossy(ty)
        );
    }
}

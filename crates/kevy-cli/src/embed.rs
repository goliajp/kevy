//! `kevy-cli --embed <dir>`: open an embedded store's data
//! directory READ-ONLY and answer the same CLI syntax, no server and
//! no downtime for the process that owns the directory.
//!
//! Mechanics: the snapshot/AOF files are COPIED to a scratch dir
//! first (the owner keeps appending to its AOF; we replay our copy to
//! its tail = a consistent point-in-time view), then opened with AOF
//! writing disabled. Verbs are the embedded RESP listener's read-only
//! whitelist, answered via `Store::dispatch_readonly`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use kevy_embedded::{Config as EmbedConfig, Store};
use kevy_resp::parse_reply;

pub fn run_embed_cli(dir: &str, command: &[Vec<u8>]) -> ExitCode {
    let src = Path::new(dir);
    if !src.is_dir() {
        eprintln!("kevy-cli: --embed: '{dir}' is not a directory");
        return ExitCode::FAILURE;
    }
    let scratch = match copy_data_files(src) {
        Ok(Some(s)) => s,
        Ok(None) => {
            eprintln!("kevy-cli: --embed: no kevy data files (dump-*.rdb / aof-*.aof) in '{dir}'");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("kevy-cli: --embed: copying data files failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let nshards = kevy_persist::read_shards_meta(&scratch.join("shards.meta"))
        .map(|m| m.n)
        .unwrap_or_else(|| count_shard_files(&scratch).max(1));
    let store = match Store::open(
        EmbedConfig::default()
            .with_persist(&scratch)
            .with_shards(nshards)
            .without_aof()
            .with_ttl_reaper_manual(),
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kevy-cli: --embed: open failed: {e}");
            let _ = std::fs::remove_dir_all(&scratch);
            return ExitCode::FAILURE;
        }
    };
    let code = if command.is_empty() {
        embed_repl(&store, dir, nshards)
    } else {
        run_embed_once(&store, command)
    };
    drop(store);
    let _ = std::fs::remove_dir_all(&scratch);
    code
}

fn run_embed_once(store: &Store, command: &[Vec<u8>]) -> ExitCode {
    let mut out = Vec::new();
    store.dispatch_readonly(command, &mut out);
    let (text, is_err) = format_reply_bytes(&out);
    println!("{text}");
    if is_err { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

fn embed_repl(store: &Store, dir: &str, nshards: usize) -> ExitCode {
    println!("kevy-cli --embed {dir} (read-only point-in-time view, {nshards} shard(s))");
    let stdin = std::io::stdin();
    loop {
        print!("embed> ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => return ExitCode::SUCCESS,
            Ok(_) => {}
            Err(_) => return ExitCode::FAILURE,
        }
        let argv: Vec<Vec<u8>> = line.split_whitespace().map(|t| t.as_bytes().to_vec()).collect();
        if argv.is_empty() {
            continue;
        }
        if argv[0].eq_ignore_ascii_case(b"quit") || argv[0].eq_ignore_ascii_case(b"exit") {
            return ExitCode::SUCCESS;
        }
        let mut out = Vec::new();
        store.dispatch_readonly(&argv, &mut out);
        println!("{}", format_reply_bytes(&out).0);
    }
}

/// Copy `dump-*.rdb`, `aof-*.aof` and `shards.meta` into a scratch
/// dir. `Ok(None)` = the dir holds no kevy data files at all.
fn copy_data_files(src: &Path) -> std::io::Result<Option<PathBuf>> {
    let scratch = kevy_tmpdir::unique_dir("cli-embed");
    let mut copied = 0usize;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let n = name.to_string_lossy();
        let is_data = (n.starts_with("dump-") && n.ends_with(".rdb"))
            || (n.starts_with("aof-") && n.ends_with(".aof"))
            || n == "shards.meta";
        if is_data {
            std::fs::copy(entry.path(), scratch.join(&name))?;
            copied += 1;
        }
    }
    if copied == 0 {
        let _ = std::fs::remove_dir_all(&scratch);
        return Ok(None);
    }
    Ok(Some(scratch))
}

fn count_shard_files(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    let n = e.file_name();
                    let n = n.to_string_lossy().into_owned();
                    n.starts_with("dump-") && n.ends_with(".rdb")
                })
                .count()
        })
        .unwrap_or(0)
}

/// Render raw RESP reply bytes with the CLI's usual formatter; second
/// element = "was a RESP error".
fn format_reply_bytes(buf: &[u8]) -> (String, bool) {
    match parse_reply(buf) {
        Ok(Some((reply, _))) => {
            let is_err = matches!(reply, kevy_resp::Reply::Error(_));
            (kevy_cli::format_reply(&reply, 0), is_err)
        }
        _ => (format!("(unparseable reply: {} bytes)", buf.len()), true),
    }
}

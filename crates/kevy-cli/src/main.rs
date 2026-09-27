//! kevy-cli — redis-cli for [kevy] or any RESP server, plus kevy's tools.
//!
//! Pure Rust, zero third-party dependencies (just [kevy-resp] + `std`).
//!
//! ```text
//! kevy-cli [options] [command args...]           # what redis-cli does
//! kevy-cli [options] --kevy <tool> [tool args]   # sql, export, doctor, tables, …
//! ```
//!
//! The redis-cli half lives in the `kevy_cli::rcli` library module; its
//! acceptance is a byte-for-byte comparison with redis-cli (`bench/cligate.py`).
//!
//! [kevy]: https://crates.io/crates/kevy
//! [kevy-resp]: https://crates.io/crates/kevy-resp
#![forbid(unsafe_code)]

use std::process::ExitCode;

mod embed;

fn main() -> ExitCode {
    use std::os::unix::ffi::OsStringExt;
    let raw: Vec<Vec<u8>> = std::env::args_os().skip(1).map(OsStringExt::into_vec).collect();
    let args: Vec<String> = raw.iter().map(|a| String::from_utf8_lossy(a).into_owned()).collect();
    // `--embed <dir>`: read-only point-in-time view of an embedded store's
    // data directory. No server, no downtime.
    if args.first().is_some_and(|a| a == "--embed") {
        let Some(dir) = args.get(1).cloned() else {
            eprintln!("kevy-cli: --embed needs a data directory (kevy-cli --embed /data/kevy)");
            return ExitCode::FAILURE;
        };
        let cmd: Vec<Vec<u8>> = args[2..].iter().map(|s| s.clone().into_bytes()).collect();
        return embed::run_embed_cli(&dir, &cmd);
    }
    // The bare tool words shipped before --kevy (deprecated through 6.x).
    if let Some(code) = kevy_cli::route_tool(&args) {
        return code;
    }
    ExitCode::from(kevy_cli::rcli::run(&raw))
}

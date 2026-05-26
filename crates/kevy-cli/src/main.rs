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

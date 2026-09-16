//! `main` (rc:11270-11533): defaults, options, environment, then the first
//! enabled mode.

use super::opts::Opts;
use super::opts_parse::{Parsed, parse};
use super::session::{Connect, Session, eprint_bytes};
use std::io::{BufRead, IsTerminal};

/// Run redis-cli with `args` (without the program name); the exit code.
pub fn run(args: &[Vec<u8>]) -> u8 {
    let stdout_tty = std::io::stdout().is_terminal() || std::env::var_os("FAKETTY").is_some();
    let (mut opts, first) = match parse(args, stdout_tty) {
        Parsed::Exit(code) => return code,
        Parsed::Run(opts, first) => (*opts, first),
    };
    if opts.auth.is_none() {
        opts.auth = auth_from_env();
    }
    if opts.askpass {
        opts.auth = ask_password();
    }
    if let Some(flag) = unimplemented_mode(&opts) {
        eprint_bytes(&[b"kevy-cli: ", flag.as_bytes(), b" is not implemented yet\n"]);
        return 1;
    }
    let command = &args[first..];
    let mut session = Session::new(opts);
    if command.is_empty() {
        session.connect(Connect::IfNeeded);
        return super::repl::run(&mut session);
    }
    session.connect(Connect::Quiet);
    super::oneshot::run(&mut session, command)
}

/// `parseEnv`: the password from the environment, kevy's name first.
fn auth_from_env() -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStringExt;
    ["KEVYCLI_AUTH", "VALKEYCLI_AUTH", "REDISCLI_AUTH"]
        .iter()
        .find_map(std::env::var_os)
        .map(OsStringExt::into_vec)
}

/// `--askpass`: a line from standard input. The masked terminal prompt is
/// P1; off a terminal redis-cli reads the line without a prompt, as here.
fn ask_password() -> Option<Vec<u8>> {
    let mut line = Vec::new();
    match std::io::stdin().lock().read_until(b'\n', &mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => {
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            Some(line)
        }
    }
}

/// The modes later phases implement, in redis-cli's dispatch order.
fn unimplemented_mode(o: &Opts) -> Option<&'static str> {
    let m = &o.modes;
    [
        (m.cluster.is_some(), "--cluster"),
        (m.latency, "--latency"),
        (m.latency_dist, "--latency-dist"),
        (m.vset_recall.is_some(), "--vset-recall"),
        (m.replica, "--replica"),
        (m.getrdb || m.functions_rdb, "--rdb"),
        (m.pipe, "--pipe"),
        (m.bigkeys, "--bigkeys"),
        (m.memkeys, "--memkeys"),
        (m.keystats, "--keystats"),
        (m.hotkeys, "--hotkeys"),
        (m.stat, "--stat"),
        (m.scan, "--scan"),
        (m.lru_test.is_some(), "--lru-test"),
        (m.intrinsic_latency.is_some(), "--intrinsic-latency"),
        (m.test_hint.is_some(), "--test_hint"),
        (m.test_hint_file.is_some(), "--test_hint_file"),
        (m.eval.is_some(), "--eval"),
        (o.cluster_mode, "-c"),
    ]
    .into_iter()
    .find_map(|(on, flag)| on.then_some(flag))
}

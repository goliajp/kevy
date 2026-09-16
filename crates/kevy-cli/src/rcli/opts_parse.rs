//! `parseOptions` (rc:2706-3158): argv to [`Opts`], in redis-cli's order
//! and with its messages, so a script's stderr and exit code are the same.

use super::cnum::{atof, atoi, atoll, strtod_full, strtoll};
use super::format::Output;
use super::help::usage;
use super::opts::Opts;
use super::splitargs::split_args;
use super::uri::apply_uri;
use std::io::Write;

/// What parsing decided.
pub(crate) enum Parsed {
    /// Run with these options; the command starts at this argv index.
    Run(Box<Opts>, usize),
    /// Exit now with this code; any message has been written.
    Exit(u8),
}

/// One flag's effect on the scan.
pub(crate) enum Step {
    /// Continue at this argv index.
    Next(usize),
    /// A non-flag: the command starts here.
    Stop,
    /// Exit with this code.
    Exit(u8),
}

/// Print `parts` to stderr as one line.
pub(crate) fn fail(parts: &[&[u8]]) -> Step {
    let mut line = parts.concat();
    line.push(b'\n');
    let _ = std::io::stderr().write_all(&line); // stderr is where exit reasons go; nothing left to tell if it is closed
    Step::Exit(1)
}

/// Parse `argv` (without the program name).
pub(crate) fn parse(argv: &[Vec<u8>], stdout_is_tty: bool) -> Parsed {
    let mut o = Opts::defaults(stdout_is_tty);
    let mut i = 0;
    while i < argv.len() {
        match step(&mut o, argv, i) {
            Step::Next(n) => i = n,
            Step::Stop => break,
            Step::Exit(code) => return Parsed::Exit(code),
        }
    }
    match post_checks(&o) {
        Step::Exit(code) => Parsed::Exit(code),
        _ => Parsed::Run(Box::new(o), i),
    }
}

/// The value of the flag at `i`, if it is not the last argument.
pub(crate) fn value(argv: &[Vec<u8>], i: usize) -> Option<&[u8]> {
    argv.get(i + 1).map(Vec::as_slice)
}

fn step(o: &mut Opts, argv: &[Vec<u8>], i: usize) -> Step {
    let flag = argv[i].as_slice();
    let found = connection_flag(o, argv, i)
        .or_else(|| output_flag(o, argv, i))
        .or_else(|| super::opts_modes::mode_flag(o, argv, i))
        .or_else(|| super::opts_modes::cluster_flag(o, argv, i))
        .or_else(|| super::opts_modes::tls_flag(flag));
    if let Some(s) = found {
        return s;
    }
    if o.modes.cluster.as_ref().is_some_and(|c| c.len() == 1) && flag.first() != Some(&b'-') {
        return super::opts_modes::late_cluster_args(o, argv, i);
    }
    if o.modes.cluster.is_some() && flag.first() != Some(&b'-') {
        return Step::Next(i + 1);
    }
    if flag.first() == Some(&b'-') {
        return fail(&[b"Unrecognized option or bad number of args for: '", flag, b"'"]);
    }
    Step::Stop
}

// LOC-WAIVER: a dispatch table — one arm per flag, each a single assignment.
fn connection_flag(o: &mut Opts, argv: &[Vec<u8>], i: usize) -> Option<Step> {
    let next = Some(Step::Next(i + 1));
    let takes = Some(Step::Next(i + 2));
    let v = value(argv, i);
    match (argv[i].as_slice(), v) {
        (b"-h", Some(h)) => o.host = h.to_vec(),
        (b"-h", None) | (b"--help", _) => return Some(Step::Exit(usage(0))),
        (b"-x", _) => o.stdin_lastarg = true,
        (b"-X", Some(tag)) => o.stdin_tag = Some(tag.to_vec()),
        (b"-p", Some(p)) => return Some(port(o, p).unwrap_or(Step::Next(i + 2))),
        (b"-t", Some(t)) => match strtod_full(t) {
            Some(s) if s >= 0.0 && !s.is_nan() => o.connect_timeout = (s > 0.0).then_some(s),
            _ => return Some(fail(&[b"Invalid connection timeout for -t."])),
        },
        (b"-s", Some(s)) => o.socket = Some(s.to_vec()),
        (b"-r", Some(r)) => o.repeat = atoll(r),
        (b"-i", Some(s)) => o.interval_us = (atof(s) * 1_000_000.0) as u64,
        (b"-n", Some(n)) => o.input_dbnum = atoi(n),
        (b"--no-auth-warning", _) => o.no_auth_warning = true,
        (b"--askpass", _) => o.askpass = true,
        (b"-a" | b"--pass", Some(a)) => o.auth = Some(a.to_vec()),
        (b"--user", Some(u)) => o.user = Some(u.to_vec()),
        (b"-u", Some(u)) => return Some(apply_uri(o, u).unwrap_or(Step::Next(i + 2))),
        (b"-c", _) => o.cluster_mode = true,
        (b"-e", _) => o.set_errcode = true,
        (b"--verbose", _) => o.verbose = true,
        (b"-4", _) => o.prefer_ipv4 = true,
        (b"-6", _) => o.prefer_ipv6 = true,
        (b"-2", _) => o.resp2 = true,
        (b"-3", _) => o.resp3 = 1,
        (b"--name", Some(n)) => o.client_name = Some(n.to_vec()),
        (b"-v" | b"--version", _) => {
            println!("kevy-cli {}", env!("CARGO_PKG_VERSION"));
            return Some(Step::Exit(0));
        }
        _ => return None,
    }
    if matches!(
        argv[i].as_slice(),
        b"-x"
            | b"--no-auth-warning"
            | b"--askpass"
            | b"-c"
            | b"-e"
            | b"--verbose"
            | b"-4"
            | b"-6"
            | b"-2"
            | b"-3"
    ) {
        next
    } else {
        takes
    }
}

/// `-p`: `atoi`, range-checked. DEV-002: text that is not a number is
/// refused, where redis-cli would take it as port 0.
fn port(o: &mut Opts, p: &[u8]) -> Option<Step> {
    let (n, used) = strtoll(p);
    if used == 0 || !(0..=65535).contains(&n) {
        return Some(fail(&[b"Invalid server port."]));
    }
    o.port = n as i32;
    None
}

// LOC-WAIVER: a dispatch table — one arm per output flag.
fn output_flag(o: &mut Opts, argv: &[Vec<u8>], i: usize) -> Option<Step> {
    let v = value(argv, i);
    match (argv[i].as_slice(), v) {
        (b"--raw", _) => o.output = Output::Raw,
        (b"--no-raw", _) => o.output = Output::Standard,
        (b"--quoted-input", _) => o.quoted_input = true,
        (b"--csv", _) => o.output = Output::Csv,
        (b"--json" | b"--quoted-json", _) => {
            if o.resp3 == 0 {
                o.resp3 = 2;
            }
            o.output = if argv[i] == b"--json" { Output::Json } else { Output::QuotedJson };
        }
        (b"-d", Some(d)) => {
            o.delims.multibulk = d.to_vec();
            return Some(Step::Next(i + 2));
        }
        (b"-D", Some(d)) => {
            o.delims.reply = d.to_vec();
            return Some(Step::Next(i + 2));
        }
        (b"--show-pushes", Some(yn)) => {
            match yn.first().map(u8::to_ascii_lowercase) {
                Some(b'n') => o.push_output = false,
                Some(b'y') => o.push_output = true,
                _ => {
                    let _ = fail(&[
                        b"Unknown --show-pushes value '",
                        yn,
                        b"' (valid: '[y]es', '[n]o')",
                    ]);
                }
            }
            return Some(Step::Next(i + 2));
        }
        _ => return None,
    }
    Some(Step::Next(i + 1))
}

/// `--quoted-input` / `--quoted-pattern`: exactly one token after unquoting.
pub(crate) fn unquote(arg: &[u8]) -> Option<Vec<u8>> {
    match split_args(arg) {
        Some(mut v) if v.len() == 1 => v.pop(),
        _ => None,
    }
}

/// The mutual-exclusion checks and the password warning, in redis-cli's order.
fn post_checks(o: &Opts) -> Step {
    if o.socket.is_some() && o.cluster_mode {
        return fail(&[b"Options -c and -s are mutually exclusive."]);
    }
    if o.resp2 && o.resp3 == 1 {
        return fail(&[b"Options -2 and -3 are mutually exclusive."]);
    }
    if o.modes.eval_ldb && o.modes.eval.is_none() {
        let _ = fail(&[b"Options --ldb and --ldb-sync-mode require --eval."]);
        return fail(&[b"Try kevy-cli --help for more information."]);
    }
    if !o.no_auth_warning && o.auth.is_some() {
        let _ = fail(&[b"Warning: Using a password with '-a' or '-u' option on the command line interface may not be safe."]);
    }
    if o.modes.functions_rdb && o.modes.getrdb {
        return fail(&[b"Option --functions-rdb and --rdb are mutually exclusive."]);
    }
    if o.stdin_lastarg && o.stdin_tag.is_some() {
        return fail(&[b"Options -x and -X are mutually exclusive."]);
    }
    if o.prefer_ipv4 && o.prefer_ipv6 {
        return fail(&[b"Options -4 and -6 are mutually exclusive."]);
    }
    Step::Next(0)
}

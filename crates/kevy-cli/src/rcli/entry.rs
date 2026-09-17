//! Startup: defaults, options, environment, then the first enabled mode.

use super::opts_parse::{Parsed, parse};
use super::session::{Connect, Session, eprint_bytes};
use std::io::IsTerminal;

/// Run redis-cli with `args` (without the program name); the exit code.
///
/// Output goes to the process's stdout and stderr, as redis-cli's does.
///
/// # Examples
///
/// ```
/// // `--version` answers without a server and exits 0.
/// assert_eq!(kevy_cli::rcli::run(&[b"--version".to_vec()]), 0);
/// ```
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
    let command = &args[first..];
    let mut session = Session::new(opts);
    if let Some(input) = session.opts.modes.test_hint.clone() {
        return super::hint_modes::print_hint(&mut session, &input);
    }
    if let Some(path) = session.opts.modes.test_hint_file.clone() {
        return super::hint_modes::check_hints(&mut session, &path);
    }
    if let Some(code) = super::modes::dispatch::run(&mut session) {
        return code;
    }
    if let Some(file) = session.opts.modes.eval.clone() {
        if session.opts.modes.eval_ldb {
            eprint_bytes(&[b"kevy-cli: --ldb is not implemented yet\n"]);
            return 1;
        }
        return super::modes::eval::run(&mut session, &file, command);
    }
    if command.is_empty() {
        session.connect(Connect::Report);
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

/// `--askpass`: the password typed at a masked prompt, or a line of piped
/// input without one.
fn ask_password() -> Option<Vec<u8>> {
    super::input::Input::read_secret(b"Please input password: ")
}

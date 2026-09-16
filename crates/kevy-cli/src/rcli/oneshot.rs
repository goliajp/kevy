//! A command on the command line: `noninteractive` (rc:3709-3751).

use super::opts_parse::unquote;
use super::send::Read;
use super::session::{Session, eprint_bytes};
use std::io::Read as _;

/// Run the command at `args`; the process exit code.
pub(crate) fn run(s: &mut Session, args: &[Vec<u8>]) -> u8 {
    let mut argv = if s.opts.quoted_input {
        match args.iter().map(|a| unquote(a)).collect::<Option<Vec<_>>>() {
            Some(v) => v,
            None => {
                super::send::write_out(b"Invalid quoted string\n");
                return 1;
            }
        }
    } else {
        args.to_vec()
    };
    if s.opts.stdin_lastarg {
        argv.push(stdin_all());
    } else if let Some(tag) = s.opts.stdin_tag.clone() {
        match argv.iter().position(|a| *a == tag) {
            Some(i) => argv[i] = stdin_all(),
            None => {
                eprint_bytes(&[b"Using -X option but stdin tag not match.\n"]);
                return 1;
            }
        }
    }
    let repeat = s.opts.repeat;
    let ok = s.issue(&argv, repeat);
    while s.pubsub_mode {
        if let Read::Failed = s.read_reply(false) {
            s.print_context_error();
            return 1;
        }
    }
    u8::from(!ok)
}

/// `readArgFromStdin`: all of standard input, binary-safe.
fn stdin_all() -> Vec<u8> {
    let mut buf = Vec::new();
    if let Err(e) = std::io::stdin().lock().read_to_end(&mut buf) {
        let text = super::conn::strerror(&e);
        eprint_bytes(&[b"Reading from standard input: ", text.as_bytes(), b"\n"]);
        std::process::exit(1);
    }
    buf
}

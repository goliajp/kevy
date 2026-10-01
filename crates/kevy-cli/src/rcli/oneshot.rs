//! A command on the command line, run once (or `-r` times).

use super::opts_parse::unquote;
use super::send::Read;
use super::session::{Session, eprint_bytes};

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
    if !with_stdin(&s.opts, &mut argv) {
        return 1;
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

/// `-x`: standard input as one more argument; `-X tag`: in place of the
/// argument equal to `tag`. `false` after saying the tag is missing.
pub(crate) fn with_stdin(opts: &super::opts::Opts, argv: &mut Vec<Vec<u8>>) -> bool {
    if opts.stdin_lastarg {
        argv.push(stdin_all());
    } else if let Some(tag) = &opts.stdin_tag {
        match argv.iter().position(|a| a == tag) {
            Some(i) => argv[i] = stdin_all(),
            None => {
                eprint_bytes(&[b"Using -X option but stdin tag not match.\n"]);
                return false;
            }
        }
    }
    true
}

/// All of standard input, binary-safe (`-x` / `-X`).
fn stdin_all() -> Vec<u8> {
    match super::input::read_all_typed() {
        Ok(all) => all,
        Err(e) => {
            let text = super::conn::strerror(&e);
            eprint_bytes(&[b"Reading from standard input: ", text.as_bytes(), b"\n"]);
            std::process::exit(1);
        }
    }
}

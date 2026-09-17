//! `--eval <file> [key ...] , [arg ...]`: run a Lua script file with EVAL.

use crate::rcli::send::write_out;
use crate::rcli::session::{Connect, Session, eprint_bytes};
use std::os::unix::ffi::OsStrExt;

/// Send the script once (or `-r` times) and print its reply as a command's;
/// with `--ldb`, debug it at a `lua debugger>` prompt instead.
pub(crate) fn run(s: &mut Session, file: &[u8], rest: &[Vec<u8>]) -> u8 {
    s.connect(Connect::Quiet);
    let debug = s.opts.modes.eval_ldb;
    loop {
        if debug {
            write_out(BANNER);
        }
        let script = match std::fs::read(std::ffi::OsStr::from_bytes(file)) {
            Ok(text) => text,
            Err(e) => {
                let why = crate::rcli::conn::strerror(&e);
                eprint_bytes(&[b"Can't open file '", file, b"': ", why.as_bytes(), b"\n"]);
                return 1;
            }
        };
        if debug {
            s.ldb.sync = s.opts.modes.eval_ldb_sync;
            let mode: &[u8] = if s.ldb.sync { b"sync" } else { b"yes" };
            let _ = s.request(&[b"SCRIPT", b"DEBUG", mode]); // a refusal shows as the EVAL's reply
            s.ldb.armed = true;
        }
        let repeat = s.opts.repeat;
        let ok = s.issue(&eval_argv(script, rest), repeat);
        if !debug {
            return u8::from(!ok);
        }
        if s.ldb.ended {
            // Ended at once: the script did not compile.
            s.ldb.ended = false;
            write_out(b"Eval debugging session can't start:\n");
            let _ = s.read_reply(false);
            return u8::from(!ok);
        }
        let code = crate::rcli::repl::run(s);
        if !std::mem::take(&mut s.ldb.restart) {
            return code;
        }
        s.connect(Connect::Report);
        write_out(b"\n");
    }
}

const BANNER: &[u8] = b"Lua debugging session started, please use:\n\
quit    -- End the session.\n\
restart -- Restart the script in debug mode again.\n\
help    -- Show Lua script debugging commands.\n\n";

/// `EVAL <script> <numkeys> keys... args...`: the first `,` standing alone
/// ends the keys; a comma inside a word is part of it.
fn eval_argv(script: Vec<u8>, rest: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let split = rest.iter().position(|a| a == b",").unwrap_or(rest.len());
    let keys = &rest[..split];
    let args = rest.get(split + 1..).unwrap_or_default();
    let mut argv = vec![b"EVAL".to_vec(), script, keys.len().to_string().into_bytes()];
    argv.extend_from_slice(keys);
    argv.extend_from_slice(args);
    argv
}

#[cfg(test)]
mod tests {
    use super::eval_argv;

    fn words(line: &str) -> Vec<Vec<u8>> {
        line.split(' ').filter(|w| !w.is_empty()).map(|w| w.as_bytes().to_vec()).collect()
    }

    #[test]
    fn a_standalone_comma_splits_keys_from_args() {
        let argv = |rest: &str| {
            String::from_utf8_lossy(&eval_argv(b"S".to_vec(), &words(rest)).join(&b' '))
                .into_owned()
        };
        assert_eq!(argv("k1 k2 , a1 a2"), "EVAL S 2 k1 k2 a1 a2");
        assert_eq!(argv("k1, a1"), "EVAL S 2 k1, a1");
        assert_eq!(argv(", a1"), "EVAL S 0 a1");
        assert_eq!(argv("k , a , b"), "EVAL S 1 k a , b");
        assert_eq!(argv("k ,"), "EVAL S 1 k");
        assert_eq!(argv(""), "EVAL S 0");
    }
}

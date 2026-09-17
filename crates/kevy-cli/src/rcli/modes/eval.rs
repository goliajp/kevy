//! `--eval <file> [key ...] , [arg ...]`: run a Lua script file with EVAL.

use crate::rcli::session::{Connect, Session, eprint_bytes};
use std::os::unix::ffi::OsStrExt;

/// Send the script once (or `-r` times) and print its reply as a command's.
pub(crate) fn run(s: &mut Session, file: &[u8], rest: &[Vec<u8>]) -> u8 {
    let script = match std::fs::read(std::ffi::OsStr::from_bytes(file)) {
        Ok(text) => text,
        Err(e) => {
            let why = crate::rcli::conn::strerror(&e);
            eprint_bytes(&[b"Can't open file '", file, b"': ", why.as_bytes(), b"\n"]);
            return 1;
        }
    };
    s.connect(Connect::Quiet);
    let repeat = s.opts.repeat;
    u8::from(!s.issue(&eval_argv(script, rest), repeat))
}

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

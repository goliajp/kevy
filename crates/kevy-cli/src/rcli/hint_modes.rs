//! `--test_hint` and `--test_hint_file`: the REPL's hints, printed or
//! checked without a terminal.

use super::docs::model::Docs;
use super::session::{Session, eprint_bytes};
use super::splitargs::split_args;

/// Print the hint for `input` and a line break; exit 0.
///
/// redis-cli crashes when the input names no command; this prints an empty
/// line (DEV-013).
pub(crate) fn print_hint(s: &mut Session, input: &[u8]) -> u8 {
    let hint = s.docs().hint(input).unwrap_or_default();
    super::send::write_out(&[hint.as_slice(), b"\n"].concat());
    0
}

/// Check each `"input" "expected hint"` line of `path` (`#` lines skipped);
/// the exit code is the number of failures.
pub(crate) fn check_hints(s: &mut Session, path: &[u8]) -> u8 {
    use std::os::unix::ffi::OsStrExt;
    let text = match std::fs::read(std::ffi::OsStr::from_bytes(path)) {
        Ok(t) => t,
        Err(e) => {
            let why = super::conn::strerror(&e);
            eprint_bytes(&[b"Can't open file '", path, b"': ", why.as_bytes(), b"\n"]);
            return 255;
        }
    };
    let docs = s.docs();
    let (mut pass, mut fail) = (0u64, 0u64);
    for line in text.split(|&b| b == b'\n').filter(|l| l.first() != Some(&b'#')) {
        let Some(argv) = split_args(line).filter(|a| !a.is_empty()) else { continue };
        let [input, expected, ..] = argv.as_slice() else {
            eprint_bytes(&[b"Missing expected hint for input '", &argv[0], b"'\n"]);
            return 255;
        };
        if check_one(&docs, input, expected, s.opts.verbose) {
            pass += 1;
        } else {
            fail += 1;
        }
    }
    let verdict = if fail == 0 { "SUCCESS" } else { "FAILURE" };
    super::send::write_out(format!("{verdict}: {pass}/{} passed\n", pass + fail).as_bytes());
    fail as u8
}

/// One case; `true` when the hint is the expected one. Failures are reported.
fn check_one(docs: &Docs, input: &[u8], expected: &[u8], verbose: bool) -> bool {
    let hint = docs.hint(input);
    if verbose {
        let shown = hint.as_deref().unwrap_or(b"(null)");
        let parts: [&[u8]; 7] =
            [b"Input: '", input, b"', Expected: '", expected, b"', Hint: '", shown, b"'\n"];
        super::send::write_out(&parts.concat());
    }
    match hint.map(without_trailing_spaces) {
        Some(h) if h == expected => true,
        got => {
            let shown = got.unwrap_or_else(|| b"(null)".to_vec());
            eprint_bytes(&[
                b"Test case '",
                input,
                b"' FAILED: expected '",
                expected,
                b"', got '",
                &shown,
                b"'\n",
            ]);
            false
        }
    }
}

/// Trailing spaces do not count; trailing tabs are part of the hint.
fn without_trailing_spaces(mut hint: Vec<u8>) -> Vec<u8> {
    while hint.last() == Some(&b' ') {
        hint.pop();
    }
    hint
}

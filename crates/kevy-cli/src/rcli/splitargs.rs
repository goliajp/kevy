//! `sdssplitargs`: how redis-cli turns a line into arguments.
//!
//! A byte-for-byte port of hiredis `hi_sdssplitargs` (redis 8.10.1,
//! `deps/hiredis/sds.c`), because the REPL, `--quoted-input` and
//! `--quoted-pattern` all promise redis-cli's quoting, and a user's muscle
//! memory for `"a\x00b"` or `'it\'s'` is that function's behaviour, quirks
//! included: a closing quote must be followed by a space or the end, an
//! unquoted `"` opens a quoted run in the middle of a token, and a NUL ends
//! the line the way it ends a C string.

/// Split `line` into arguments, or `None` for unbalanced quotes or a closing
/// quote followed by something other than a space.
pub(crate) fn split_args(line: &[u8]) -> Option<Vec<Vec<u8>>> {
    let end = line.iter().position(|&b| b == 0).unwrap_or(line.len());
    let line = &line[..end];
    let mut args = Vec::new();
    let mut p = 0;
    loop {
        while p < line.len() && is_c_space(line[p]) {
            p += 1;
        }
        if p == line.len() {
            return Some(args);
        }
        let (token, next) = token_at(line, p)?;
        args.push(token);
        p = next;
    }
}

/// One token starting at `p`; returns it and the index after it.
fn token_at(line: &[u8], mut p: usize) -> Option<(Vec<u8>, usize)> {
    let mut cur = Vec::new();
    let mut quote = Quote::None;
    loop {
        let c = at(line, p);
        let step = match quote {
            Quote::Double => double_quoted(line, p, &mut cur),
            Quote::Single => single_quoted(line, p, &mut cur),
            Quote::None => match c {
                b' ' | b'\n' | b'\r' | b'\t' | 0 => Step::Close(if c == 0 { p } else { p + 1 }),
                b'"' => {
                    quote = Quote::Double;
                    Step::Advance(p + 1)
                }
                b'\'' => {
                    quote = Quote::Single;
                    Step::Advance(p + 1)
                }
                _ => {
                    cur.push(c);
                    Step::Advance(p + 1)
                }
            },
        };
        match step {
            Step::Advance(next) => p = next,
            Step::Close(next) => return Some((cur, next)),
            Step::Invalid => return None,
        }
    }
}

enum Quote {
    None,
    Double,
    Single,
}

enum Step {
    Advance(usize),
    Close(usize),
    Invalid,
}

fn at(line: &[u8], i: usize) -> u8 {
    line.get(i).copied().unwrap_or(0)
}

/// Inside `"…"`: `\xHH`, the C escapes, and a closing quote that must be
/// followed by a blank or the end.
fn double_quoted(line: &[u8], p: usize, cur: &mut Vec<u8>) -> Step {
    let c = at(line, p);
    if c == b'\\'
        && at(line, p + 1) == b'x'
        && at(line, p + 2).is_ascii_hexdigit()
        && at(line, p + 3).is_ascii_hexdigit()
    {
        cur.push(hex(at(line, p + 2)) * 16 + hex(at(line, p + 3)));
        return Step::Advance(p + 4);
    }
    if c == b'\\' && at(line, p + 1) != 0 {
        cur.push(unescape(at(line, p + 1)));
        return Step::Advance(p + 2);
    }
    close_or_push(line, p, b'"', cur)
}

/// Inside `'…'`: only `\'` is an escape.
fn single_quoted(line: &[u8], p: usize, cur: &mut Vec<u8>) -> Step {
    if at(line, p) == b'\\' && at(line, p + 1) == b'\'' {
        cur.push(b'\'');
        return Step::Advance(p + 2);
    }
    close_or_push(line, p, b'\'', cur)
}

fn close_or_push(line: &[u8], p: usize, quote: u8, cur: &mut Vec<u8>) -> Step {
    match at(line, p) {
        c if c == quote => {
            let after = at(line, p + 1);
            if after != 0 && !is_c_space(after) { Step::Invalid } else { Step::Close(p + 1) }
        }
        0 => Step::Invalid,
        c => {
            cur.push(c);
            Step::Advance(p + 1)
        }
    }
}

/// C `isspace` in the "C" locale.
fn is_c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn hex(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        _ => b - b'A' + 10,
    }
}

fn unescape(b: u8) -> u8 {
    match b {
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        b'b' => 0x08,
        b'a' => 0x07,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::split_args;

    fn s(line: &str) -> Option<Vec<String>> {
        split_args(line.as_bytes())
            .map(|v| v.into_iter().map(|a| String::from_utf8_lossy(&a).into_owned()).collect())
    }

    #[test]
    fn plain_words_and_blank_runs() {
        assert_eq!(s("  set  k\tv \r\n"), Some(vec!["set".into(), "k".into(), "v".into()]));
        assert_eq!(s(""), Some(vec![]));
        assert_eq!(s("   "), Some(vec![]));
    }

    #[test]
    fn double_quotes_take_escapes() {
        // `\xZZ` is not a hex escape, so `\x` is an escaped `x`.
        assert_eq!(split_args(br#""a b\n\x41\xZZ\q""#), Some(vec![b"a b\nAxZZq".to_vec()]));
        assert_eq!(split_args(br#""\x00""#), Some(vec![vec![0]]));
    }

    #[test]
    fn single_quotes_take_only_the_quote_escape() {
        assert_eq!(s(r"'it\'s' '\n'"), Some(vec!["it's".into(), r"\n".into()]));
    }

    #[test]
    fn quote_opens_mid_token_and_must_close_before_a_space() {
        assert_eq!(s(r#"ab"c d" e"#), Some(vec!["abc d".into(), "e".into()]));
        assert_eq!(s(r#""foo"bar"#), None);
        assert_eq!(s(r#""unterminated"#), None);
        assert_eq!(s("'x"), None);
    }

    #[test]
    fn nul_ends_the_line() {
        assert_eq!(split_args(b"a b\0c"), Some(vec![b"a".to_vec(), b"b".to_vec()]));
    }
}

#[cfg(test)]
mod escape_tests {
    use super::split_args;

    #[test]
    fn every_c_escape_and_both_hex_cases() {
        assert_eq!(
            split_args(br#""\r\t\b\a\xAb\xcD""#),
            Some(vec![b"\r\t\x08\x07\xab\xcd".to_vec()])
        );
    }
}

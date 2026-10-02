//! `BITOP`'s operators against the replies Redis 8.10.2 gave to the same
//! commands. Each blank-line-separated group starts on an empty store;
//! arguments and replies escape a byte as `\xNN`, `\r`, `\n`, `\t` or
//! `\\`.

use crate::{Config, Store};

fn unescape(s: &str) -> Vec<u8> {
    let (mut out, b) = (Vec::new(), s.as_bytes());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        let (byte, width) = match b[i + 1] {
            b'r' => (b'\r', 2),
            b'n' => (b'\n', 2),
            b't' => (b'\t', 2),
            b'\\' => (b'\\', 2),
            b'x' => (u8::from_str_radix(&s[i + 2..i + 4], 16).expect("two hex digits"), 4),
            other => panic!("unknown escape \\{}", other as char),
        };
        out.push(byte);
        i += width;
    }
    out
}

#[test]
fn bitop_matches_redis() {
    let mut checked = 0;
    for group in include_str!("testdata/redis_bitop.txt").split("\n\n") {
        let s = Store::open(Config::default().with_ttl_reaper_manual()).expect("in-memory store");
        for line in group.lines() {
            let (cmd, want) = line.split_once("\t=>\t").expect("a command, then its reply");
            let argv: Vec<Vec<u8>> = cmd.split('\t').map(unescape).collect();
            let mut out = Vec::new();
            super::dispatch(&s, &argv, &mut out);
            assert_eq!(
                String::from_utf8_lossy(&out),
                String::from_utf8_lossy(&unescape(want)),
                "{cmd}"
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 53);
}

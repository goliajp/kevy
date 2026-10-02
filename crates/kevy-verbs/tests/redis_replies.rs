//! Commands against the replies Redis 8.10.2 gave to the same commands,
//! one table per family. Each blank-line-separated group starts on an
//! empty store; arguments and replies escape a byte as `\xNN`, `\r`, `\n`,
//! `\t` or `\\`.

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
            b'r' => Some((b'\r', 2)),
            b'n' => Some((b'\n', 2)),
            b't' => Some((b'\t', 2)),
            b'\\' => Some((b'\\', 2)),
            b'x' => u8::from_str_radix(&s[i + 2..i + 4], 16).ok().map(|b| (b, 4)),
            _ => None,
        }
        .expect("a known escape");
        out.push(byte);
        i += width;
    }
    out
}

fn run_table(table: &str) -> usize {
    let mut checked = 0;
    for group in table.split("\n\n") {
        let mut store = kevy_store::Store::new();
        for line in group.lines() {
            let (cmd, want) = line.split_once("\t=>\t").expect("a command, then its reply");
            let argv: Vec<Vec<u8>> = cmd.split('\t').map(unescape).collect();
            let verb = argv[0].to_ascii_uppercase();
            let mut out = Vec::new();
            let argv = kevy_resp::Argv::from(argv);
            if kevy_verbs::exec(&mut store, &verb, &argv, &mut out).is_none() {
                // the _RO geo twins are not in the verb table
                #[cfg(feature = "streams-geo")]
                kevy_verbs::geo::exec_read_only(&verb, &mut store, &argv, &mut out);
            }
            assert_eq!(
                String::from_utf8_lossy(&out),
                String::from_utf8_lossy(&unescape(want)),
                "{cmd}"
            );
            checked += 1;
        }
    }
    checked
}

/// `SET`'s options, `GETEX`, `DIGEST`, the `EXPIRE` family's conditions and
/// `LPOP` / `RPOP` with a count of 0.
#[test]
fn set_family_matches_redis() {
    assert_eq!(run_table(include_str!("data/redis_set_family.txt")), 269);
}

/// `BITCOUNT` / `BITPOS` in bytes and in bits.
#[test]
fn bitmaps_match_redis() {
    assert_eq!(run_table(include_str!("data/redis_bitmap.txt")), 98);
}

/// The geo searches' option checks, in Redis's order, and their replies.
/// A distance read back after STOREDIST is x86-64 Redis's: its aarch64
/// builds fuse multiply-adds and differ in the last digit.
#[cfg(feature = "streams-geo")]
#[test]
fn geo_searches_match_redis() {
    assert_eq!(run_table(include_str!("data/redis_geo.txt")), 86);
}

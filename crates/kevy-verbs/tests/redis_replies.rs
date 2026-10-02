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

/// `XADD` / `XTRIM` with `KEEPREF`, `DELREF` and `ACKED`.
#[cfg(feature = "streams-geo")]
#[test]
fn stream_trims_with_group_references_match_redis() {
    assert_eq!(run_table(include_str!("data/redis_stream_trim.txt")), 105);
}

/// Reference-aware trims over streams of several nodes, each case built the
/// way the Redis run built it — `n` entries, `groups` groups each reading
/// `read - 10 * g` and acknowledging the first `acked - 5 * g` — then
/// trimmed: Redis's count removed, length left, pending per group and first
/// entry.
#[cfg(feature = "streams-geo")]
#[test]
fn stream_trims_over_nodes_match_redis() {
    let run = |store: &mut kevy_store::Store, words: Vec<String>| {
        let argv: Vec<Vec<u8>> = words.into_iter().map(String::into_bytes).collect();
        let verb = argv[0].to_ascii_uppercase();
        let mut out = Vec::new();
        kevy_verbs::exec(store, &verb, &kevy_resp::Argv::from(argv), &mut out);
        out
    };
    let w = |s: &str| s.split(' ').map(str::to_owned).collect::<Vec<_>>();
    let mut checked = 0;
    for line in include_str!("data/redis_stream_trim_nodes.txt").lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let num = |i: usize| f[i].parse::<usize>().expect("a count");
        let (n, read, acked, groups) = (num(0), num(1), num(2), num(3));
        let mut s = kevy_store::Store::new();
        for i in 1..=n {
            run(&mut s, w(&format!("XADD s {i}-0 f v")));
        }
        for g in 0..groups {
            run(&mut s, w(&format!("XGROUP CREATE s g{g} 0")));
            run(&mut s, w(&format!("XREADGROUP GROUP g{g} c COUNT {} STREAMS s >", read - g * 10)));
            let ids: Vec<String> = (1..=acked - g * 5).map(|i| format!("{i}-0")).collect();
            run(&mut s, w(&format!("XACK s g{g} {}", ids.join(" "))));
        }
        let removed = run(&mut s, w(&format!("XTRIM s {}", f[4])));
        assert_eq!(removed, format!(":{}\r\n", f[5]).into_bytes(), "{line}");
        assert_eq!(run(&mut s, w("XLEN s")), format!(":{}\r\n", f[6]).into_bytes(), "{line}");
        for (g, want) in f[7].split(',').filter(|p| !p.is_empty()).enumerate() {
            let pending = run(&mut s, w(&format!("XPENDING s g{g}")));
            assert!(pending.starts_with(format!("*4\r\n:{want}\r\n").as_bytes()), "{line}");
        }
        let first = run(&mut s, w("XRANGE s - + COUNT 1"));
        match f[8] {
            "-" => assert_eq!(first, b"*0\r\n", "{line}"),
            id => assert!(
                first.starts_with(format!("*1\r\n*2\r\n${}\r\n{id}\r\n", id.len()).as_bytes()),
                "{line}"
            ),
        }
        checked += 1;
    }
    assert_eq!(checked, 40);
}

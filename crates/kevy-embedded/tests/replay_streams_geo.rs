//! Stream and geo writes made through the argv path come back from the log
//! as they were: the same entries under the same IDs, the same groups,
//! pending lists and cursors, the same stored geo results.
//!
//! Every write here is one whose effect does not depend on when it runs.
//! A `*` ID and a claim with a minimum idle time do depend on it, and the
//! log records their argv; they are left out.

#![cfg(all(feature = "persist", feature = "streams-geo"))]

use kevy_embedded::{Config, Store};

fn open(dir: &std::path::Path, shards: usize) -> Store {
    Store::open(Config::default().with_persist(dir).with_shards(shards).with_ttl_reaper_manual())
        .expect("open")
}

fn call(s: &Store, cmd: &str) -> Vec<u8> {
    let argv: Vec<Vec<u8>> = cmd.split(' ').map(|p| p.as_bytes().to_vec()).collect();
    let mut out = Vec::new();
    s.dispatch_argv(&argv, &mut out);
    out
}

fn ok(s: &Store, cmd: &str) {
    let r = call(s, cmd);
    assert!(!r.starts_with(b"-"), "{cmd}: {}", String::from_utf8_lossy(&r));
}

const STREAM_WRITES: &[&str] = &[
    "XADD s 1-1 a 1",
    "XADD s 2-0 b 2",
    "XADD s MAXLEN = 3 3-5 c 3",
    "XADD s 4-0 d 4",
    "XADD s 5-0 e 5",
    "XTRIM s MAXLEN 4",
    "XDEL s 3-5",
    "XADD s2 MINID 0 10-1 x 1",
    "XSETID s2 20-0",
    "XGROUP CREATE s g 0",
    "XGROUP CREATE s g2 $",
    "XGROUP CREATE s3 g $ MKSTREAM",
    "XGROUP CREATECONSUMER s g idle",
    "XREADGROUP GROUP g c1 COUNT 2 STREAMS s >",
    "XREADGROUP GROUP g c2 STREAMS s >",
    "XACK s g 2-0",
    "XCLAIM s g c3 0 4-0",
    "XAUTOCLAIM s g c4 0 0 COUNT 1",
    "XGROUP SETID s g2 2-0",
    "XGROUP DELCONSUMER s g c2",
];

const GEO_WRITES: &[&str] = &[
    "GEOADD g 13.361389 38.115556 Palermo 15.087269 37.502669 Catania 2.349014 48.864716 Paris",
    "GEOADD g XX CH 13.4 38.1 Palermo",
    "GEOADD g NX 0 0 Paris",
];

/// The three storing geo forms, each with a destination on the source's
/// shard; `{}` is the destination.
const GEO_STORES: &[&str] = &[
    "GEOSEARCHSTORE {} g FROMMEMBER Palermo BYRADIUS 300 km STOREDIST",
    "GEORADIUS g 15 37 300 km STORE {}",
    "GEORADIUSBYMEMBER g Paris 5000 km ASC COUNT 2 STOREDIST {}",
];

/// Run a storing form into the first destination name that shares the
/// source's shard. The others are refused, and must change nothing.
fn store_near(s: &Store, form: &str, n: usize) -> String {
    for i in 0..64 {
        let dst = format!("dst{n}-{i}");
        let r = call(s, &form.replace("{}", &dst));
        if r.starts_with(b":") {
            return dst;
        }
        assert!(r.starts_with(b"-CROSSSLOT"), "{form}: {}", String::from_utf8_lossy(&r));
        assert_eq!(call(s, &format!("EXISTS {dst}")), b":0\r\n", "a refused store wrote");
    }
    panic!("{form}: no destination on the source's shard");
}

/// Everything a reader can see of the keys written above, with the idle
/// times that an extended XPENDING reply carries blanked out.
fn state(s: &Store, dsts: &[String]) -> Vec<(String, String)> {
    let mut reads: Vec<String> = [
        "XRANGE s - +",
        "XRANGE s2 - +",
        "XRANGE s3 - +",
        "XINFO STREAM s",
        "XINFO STREAM s2",
        "XINFO GROUPS s",
        "XINFO GROUPS s3",
        "XPENDING s g",
        "XPENDING s g - + 10",
        "GEOPOS g Palermo Catania Paris",
        "ZRANGE g 0 -1 WITHSCORES",
        "DBSIZE",
    ]
    .iter()
    .map(|r| r.to_string())
    .collect();
    reads.extend(dsts.iter().map(|d| format!("ZRANGE {d} 0 -1 WITHSCORES")));
    reads
        .into_iter()
        .map(|r| {
            let reply = String::from_utf8_lossy(&call(s, &r)).into_owned();
            let reply = if r.starts_with("XPENDING s g -") { blank_idle(&reply) } else { reply };
            (r, reply)
        })
        .collect()
}

/// `[id, consumer, idle, deliveries]` rows: the idle column is when the
/// state was read, not what it is.
fn blank_idle(reply: &str) -> String {
    let mut t: Vec<&str> = reply.split("\r\n").collect();
    for i in 0..t.len() {
        if t[i] == "*4" && t.get(i + 5).is_some_and(|x| x.starts_with(':')) {
            t[i + 5] = ":idle";
        }
    }
    t.join("\r\n")
}

fn round_trip(shards: usize) {
    let dir = kevy_tmpdir::TmpDir::new("replay-streams-geo");
    let (before, dsts) = {
        let s = open(dir.path(), shards);
        for w in STREAM_WRITES.iter().chain(GEO_WRITES) {
            ok(&s, w);
        }
        let dsts: Vec<String> =
            GEO_STORES.iter().enumerate().map(|(n, f)| store_near(&s, f, n)).collect();
        (state(&s, &dsts), dsts)
    };
    let after = state(&open(dir.path(), shards), &dsts);
    for ((read, b), (_, a)) in before.iter().zip(&after) {
        assert_eq!(a, b, "{shards} shard(s), {read}: changed across the restart");
    }
    // a restart that restored nothing reads the same before and after
    // only if nothing was there; this was there
    let read = |r: &str| &after.iter().find(|(q, _)| q == r).expect("read").1;
    assert!(read("XRANGE s - +").starts_with("*3\r\n"), "{}", read("XRANGE s - +"));
    // c2 went with its entry; XAUTOCLAIM took 4-0 from c3 last
    let pending = read("XPENDING s g - + 10");
    assert!(pending.starts_with("*1\r\n*4\r\n$3\r\n4-0\r\n$2\r\nc4\r\n"), "{pending}");
    assert!(read("XINFO GROUPS s").contains("2-0"), "{}", read("XINFO GROUPS s"));
    assert!(read("GEOPOS g Palermo Catania Paris").matches("*2\r\n").count() == 3);
    for d in &dsts {
        let z = read(&format!("ZRANGE {d} 0 -1 WITHSCORES"));
        assert!(z.starts_with("*4\r\n"), "{d}: {z}");
    }
}

#[test]
fn stream_and_geo_writes_survive_a_restart() {
    round_trip(1);
}

/// With several shards a stream command must run on its stream's shard:
/// `XGROUP` and `XREADGROUP` do not name the stream first.
#[test]
fn stream_and_geo_writes_survive_a_restart_across_shards() {
    round_trip(4);
}

/// A host feeding a server's stream frames back in: each lands on its
/// stream's shard, where the reads look for it.
#[test]
fn fed_frames_land_on_their_streams_shard() {
    let dir = kevy_tmpdir::TmpDir::new("replay-streams-fed");
    let s = open(dir.path(), 4);
    for f in [
        "XADD s 1-1 f v",
        "XADD s 2-1 f w",
        "XGROUP CREATE s g 0",
        "XREADGROUP GROUP g c STREAMS s >",
        "XACK s g 1-1",
        "GEOADD g 13.361389 38.115556 Palermo",
    ] {
        let argv: Vec<Vec<u8>> = f.split(' ').map(|p| p.as_bytes().to_vec()).collect();
        s.apply_frame(&kevy_resp::Argv::from(argv));
    }
    let pending = call(&s, "XPENDING s g");
    assert_eq!(
        pending,
        b"*4\r\n:1\r\n$3\r\n2-1\r\n$3\r\n2-1\r\n*1\r\n*2\r\n$1\r\nc\r\n$1\r\n1\r\n"
    );
    assert!(call(&s, "GEOPOS g Palermo").starts_with(b"*1\r\n*2\r\n"));
}

#[test]
fn a_blocking_read_is_refused_and_a_plain_one_served() {
    let dir = kevy_tmpdir::TmpDir::new("replay-streams-block");
    let s = open(dir.path(), 1);
    ok(&s, "XADD s 1-1 f v");
    let r = call(&s, "XREAD BLOCK 0 STREAMS s 0");
    assert!(r.starts_with(b"-ERR the embedded engine cannot block"), "{r:?}");
    assert!(call(&s, "XREAD STREAMS s 0").starts_with(b"*1\r\n"));
}

//! Stream and geo writes made through the argv path come back from the log
//! as they were: the same entries under the same IDs, the same groups,
//! pending lists and cursors, the same stored geo results.
//!
//! Writes whose effect depends on when they ran, a generated `XADD` ID and
//! a claim gated on idle time, are recorded as what they did, and replay to
//! the same IDs, owners and counts.

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

fn time_dependent_round_trip(shards: usize) {
    const READS: &[&str] =
        &["XRANGE s - +", "XRANGE s2 - +", "XPENDING s g - + 10", "XINFO GROUPS s"];
    let dir = kevy_tmpdir::TmpDir::new("replay-streams-time");
    let before: Vec<String> = {
        let s = open(dir.path(), shards);
        for i in 0..4 {
            ok(&s, &format!("XADD s * f {i}"));
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        ok(&s, "XADD s MAXLEN ~ 3 * f 4");
        assert!(call(&s, "XADD s2 7-* f v").starts_with(b"$3\r\n7-0"));
        assert!(call(&s, "XADD s2 7-* f w").starts_with(b"$3\r\n7-1"));
        ok(&s, "XGROUP CREATE s g 0");
        ok(&s, "XREADGROUP GROUP g c1 STREAMS s >");
        std::thread::sleep(std::time::Duration::from_millis(120));
        let first = String::from_utf8_lossy(&call(&s, "XRANGE s - + COUNT 1")).into_owned();
        let id = first.split("\r\n").nth(3).expect("an entry").to_string();
        let claimed = call(&s, &format!("XCLAIM s g c2 100 {id} JUSTID"));
        assert!(claimed.starts_with(b"*1\r\n"), "{}", String::from_utf8_lossy(&claimed));
        // the second entry goes while pending: the claim below drops it
        // and takes the third, two records from one command
        let all = String::from_utf8_lossy(&call(&s, "XRANGE s - +")).into_owned();
        let second = all.split("\r\n").nth(11).expect("a second entry").to_string();
        assert_eq!(call(&s, &format!("XDEL s {second}")), b":1\r\n");
        let auto =
            String::from_utf8_lossy(&call(&s, "XAUTOCLAIM s g c3 100 0 COUNT 3")).into_owned();
        assert!(auto.ends_with(&format!("*1\r\n$15\r\n{second}\r\n")), "{auto}");
        READS.iter().map(|r| blank_idle(&String::from_utf8_lossy(&call(&s, r)))).collect()
    };
    let s = open(dir.path(), shards);
    for (read, b) in READS.iter().zip(&before) {
        let a = blank_idle(&String::from_utf8_lossy(&call(&s, read)));
        assert_eq!(&a, b, "{shards} shard(s), {read}: changed across the restart");
    }
    assert!(before[0].starts_with("*2\r\n"), "{}", before[0]);
    assert!(before[2].contains("c2") && before[2].contains("c3"), "{}", before[2]);
    assert!(before[2].starts_with("*2\r\n"), "the dropped entry left the list: {}", before[2]);
}

/// A generated ID and an idle-gated claim replay as they were answered.
#[test]
fn generated_ids_and_idle_claims_survive_a_restart() {
    time_dependent_round_trip(1);
    time_dependent_round_trip(4);
}

/// The idle column of the extended `XPENDING` rows.
fn idles(reply: &str) -> Vec<i64> {
    let t: Vec<&str> = reply.split("\r\n").collect();
    (0..t.len())
        .filter(|&i| t[i] == "*4" && t.get(i + 5).is_some_and(|x| x.starts_with(':')))
        .map(|i| t[i + 5][1..].parse().expect("an idle time"))
        .collect()
}

/// `XINFO CONSUMERS`' idle and inactive columns, which count from the
/// consumer's last contact and last activity, blanked; an inactive of -1
/// (never handed an entry) is kept.
fn blank_consumer_idle(reply: &str) -> String {
    let mut t: Vec<String> = reply.split("\r\n").map(str::to_string).collect();
    for i in 0..t.len() {
        if t[i] == "idle" && i + 1 < t.len() {
            t[i + 1] = ":idle".into();
        }
        if t[i] == "inactive" && i + 1 < t.len() && t[i + 1] != ":-1" {
            t[i + 1] = ":inactive".into();
        }
    }
    t.join("\r\n")
}

fn delivery_round_trip(shards: usize) {
    const READS: &[&str] = &["XPENDING s g - + 10", "XINFO GROUPS s", "XINFO CONSUMERS s g"];
    let text = |s: &Store, r: &str| String::from_utf8_lossy(&call(s, r)).into_owned();
    let read_all = |s: &Store| -> Vec<String> {
        READS.iter().map(|r| blank_consumer_idle(&blank_idle(&text(s, r)))).collect()
    };
    let dir = kevy_tmpdir::TmpDir::new("replay-streams-delivery");
    let (before, idle_before, at) = {
        let s = open(dir.path(), shards);
        for id in ["1-1", "2-1", "3-1"] {
            ok(&s, &format!("XADD s {id} f v"));
        }
        ok(&s, "XGROUP CREATE s g 0");
        ok(&s, "XGROUP CREATE s n 0");
        ok(&s, "XREADGROUP GROUP g c1 COUNT 2 STREAMS s >");
        std::thread::sleep(std::time::Duration::from_millis(100));
        ok(&s, "XREADGROUP GROUP g c2 STREAMS s >");
        ok(&s, "XREADGROUP GROUP n c9 NOACK STREAMS s >");
        ok(&s, "XREADGROUP GROUP g newbie STREAMS s 0");
        assert_eq!(call(&s, "XREADGROUP GROUP g c1 STREAMS s >"), b"*-1\r\n");
        std::thread::sleep(std::time::Duration::from_millis(300));
        let idle = idles(&text(&s, "XPENDING s g - + 10"));
        let at = std::time::Instant::now();
        let mut before = read_all(&s);
        before.push(text(&s, "XINFO GROUPS s"));
        (before, idle, at)
    };
    let s = open(dir.path(), shards);
    let idle_after = idles(&text(&s, "XPENDING s g - + 10"));
    let elapsed = at.elapsed().as_millis() as i64;
    for ((read, b), a) in READS.iter().zip(&before).zip(read_all(&s)) {
        assert_eq!(&a, b, "{shards} shard(s), {read}: changed across the restart");
    }
    assert_eq!(idle_before.len(), 3, "three pending entries: {before:?}");
    for (b, a) in idle_before.iter().zip(&idle_after) {
        let gap = a - b;
        assert!(
            (elapsed - 60..=elapsed + 60).contains(&gap),
            "{shards} shard(s): idle went {b} -> {a} over {elapsed} ms: it started over"
        );
    }
    // the NOACK group moved to the end with nothing pending, and the
    // history read made a consumer
    let groups = &before[3];
    assert!(groups.contains("$1\r\nn\r\n$9\r\nconsumers\r\n:1\r\n$7\r\npending\r\n:0"), "{groups}");
    assert!(before[2].contains("newbie"), "{}", before[2]);
}

/// An entry delivered by `XREADGROUP` keeps its delivery time across a
/// restart: its idle time goes on instead of starting over.
#[test]
fn delivery_times_survive_a_restart() {
    delivery_round_trip(1);
    delivery_round_trip(4);
}

/// One `XREADGROUP` over two streams comes back from the log stream by
/// stream: the stream that delivered keeps its pending entries and their
/// delivery times, the one that did not keeps its place, and a NOACK read
/// of both moves both. One shard, so both streams share it.
#[test]
fn a_read_of_two_streams_survives_a_restart() {
    const READS: &[&str] = &[
        "XPENDING a g - + 10",
        "XPENDING b g - + 10",
        "XINFO GROUPS a",
        "XINFO GROUPS b",
        "XINFO CONSUMERS b g",
    ];
    let text = |s: &Store, r: &str| String::from_utf8_lossy(&call(s, r)).into_owned();
    let read_all = |s: &Store| -> Vec<String> {
        READS.iter().map(|r| blank_consumer_idle(&blank_idle(&text(s, r)))).collect()
    };
    let dir = kevy_tmpdir::TmpDir::new("replay-streams-two");
    let (before, idle_before, at) = {
        let s = open(dir.path(), 1);
        for w in ["XADD a 1-1 x 1", "XADD a 2-1 x 2", "XADD b 1-1 y 1"] {
            ok(&s, w);
        }
        for g in ["XGROUP CREATE a g 0", "XGROUP CREATE b g $", "XGROUP CREATE a n 0"] {
            ok(&s, g);
        }
        ok(&s, "XGROUP CREATE b n 0");
        ok(&s, "XREADGROUP GROUP g c1 STREAMS a b > >");
        ok(&s, "XREADGROUP GROUP n c2 NOACK COUNT 1 STREAMS a b > >");
        ok(&s, "XADD b 2-1 y 2");
        // the first stream named delivers nothing, the second one entry
        ok(&s, "XREADGROUP GROUP g c3 STREAMS a b > >");
        std::thread::sleep(std::time::Duration::from_millis(300));
        let idle = idles(&text(&s, "XPENDING b g - + 10"));
        (read_all(&s), idle, std::time::Instant::now())
    };
    let s = open(dir.path(), 1);
    let idle_after = idles(&text(&s, "XPENDING b g - + 10"));
    let elapsed = at.elapsed().as_millis() as i64;
    for ((read, b), a) in READS.iter().zip(&before).zip(read_all(&s)) {
        assert_eq!(&a, b, "{read}: changed across the restart");
    }
    assert_eq!(idle_before.len(), 1, "b delivered one entry to c3: {before:?}");
    let gap = idle_after[0] - idle_before[0];
    assert!((elapsed - 60..=elapsed + 60).contains(&gap), "idle {idle_before:?} -> {idle_after:?}");
    assert!(before[4].contains("c3"), "{}", before[4]);
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

/// `(consumer, idle)` pairs of `XINFO CONSUMERS s g`, by name, and when
/// they were read.
fn seen(s: &Store) -> (Vec<(String, i64)>, std::time::Instant) {
    let reply = String::from_utf8_lossy(&call(s, "XINFO CONSUMERS s g")).into_owned();
    let t: Vec<&str> = reply.split("\r\n").collect();
    let mut out: Vec<(String, i64)> = (0..t.len())
        .filter(|&i| t[i] == "name" && t.get(i + 7) == Some(&"idle"))
        .map(|i| (t[i + 2].to_string(), t[i + 8][1..].parse().expect("an idle time")))
        .collect();
    out.sort();
    (out, std::time::Instant::now())
}

/// Each consumer's idle time went on by what passed since `before.1`,
/// except `polled`'s, whose last contact was an empty read.
fn assert_went_on(
    before: &(Vec<(String, i64)>, std::time::Instant),
    after: &[(String, i64)],
    polled: Option<&str>,
) {
    let elapsed = before.1.elapsed().as_millis() as i64;
    assert_eq!(before.0.len(), 6, "six consumers: {before:?}");
    for ((name, b), (other, a)) in before.0.iter().zip(after) {
        assert_eq!(name, other);
        let gap = a - b;
        if polled == Some(name.as_str()) {
            // its last contact was an empty read, which is not recorded:
            // it comes back from its last delivering read, well before
            assert!(gap > elapsed + 60, "{name}: the empty read was recorded ({b} -> {a})");
            continue;
        }
        assert!(
            (elapsed - 80..=elapsed + 80).contains(&gap),
            "{name}: idle went {b} -> {a} over {elapsed} ms: it started over instead of going on"
        );
    }
}

/// A consumer's last contact with its group comes back from the log as
/// it was, however the consumer came about or was last seen, from the log
/// as written and from the log a rewrite compacts it to. A read that
/// delivered nothing is not recorded, so a consumer last seen by one comes
/// back with its earlier contact.
#[test]
fn consumer_seen_times_survive_a_restart() {
    let dir = kevy_tmpdir::TmpDir::new("replay-streams-seen");
    let first = {
        let s = open(dir.path(), 1);
        for id in ["1-1", "2-1", "3-1"] {
            ok(&s, &format!("XADD s {id} f v"));
        }
        ok(&s, "XGROUP CREATE s g 0");
        ok(&s, "XREADGROUP GROUP g reader COUNT 2 STREAMS s >");
        ok(&s, "XGROUP CREATECONSUMER s g made");
        std::thread::sleep(std::time::Duration::from_millis(60));
        ok(&s, "XREADGROUP GROUP g idler STREAMS s 0");
        ok(&s, "XCLAIM s g claimer 0 1-1 JUSTID");
        ok(&s, "XAUTOCLAIM s g auto 0 2-1 COUNT 1 JUSTID");
        std::thread::sleep(std::time::Duration::from_millis(60));
        ok(&s, "XREADGROUP GROUP g late COUNT 1 STREAMS s >");
        assert_eq!(call(&s, "XREADGROUP GROUP g reader STREAMS s >"), b"*-1\r\n");
        std::thread::sleep(std::time::Duration::from_millis(300));
        seen(&s)
    };
    let second = {
        let s = open(dir.path(), 1);
        assert_went_on(&first, &seen(&s).0, Some("reader"));
        let second = seen(&s);
        s.rewrite_aof().expect("rewrite");
        second
    };
    let s = open(dir.path(), 1);
    assert_went_on(&second, &seen(&s).0, None);
}

/// The internal record verb is refused from a caller of `dispatch_argv`
/// and changes nothing; fed back as a frame, it is applied.
#[test]
fn the_internal_record_verb_is_refused_from_a_caller_and_applied_from_a_frame() {
    let dir = kevy_tmpdir::TmpDir::new("replay-streams-internal");
    let s = open(dir.path(), 1);
    ok(&s, "XGROUP CREATE s g $ MKSTREAM");
    let want = format!("-{}\r\n", kevy_verbs::aof::INTERNAL_REFUSAL);
    assert_eq!(String::from_utf8_lossy(&call(&s, "XINTERNAL.CONSUMERSEEN s g c 1")), want);
    assert_eq!(call(&s, "XINFO CONSUMERS s g"), b"*0\r\n", "a refused record made a consumer");
    let frame: Vec<Vec<u8>> = ["XINTERNAL.CONSUMERSEEN", "s", "g", "c", "1"]
        .iter()
        .map(|p| p.as_bytes().to_vec())
        .collect();
    s.apply_frame(&kevy_resp::Argv::from(frame));
    let consumers = String::from_utf8_lossy(&call(&s, "XINFO CONSUMERS s g")).into_owned();
    assert!(consumers.starts_with("*1\r\n") && consumers.contains("$1\r\nc\r\n"), "{consumers}");
}

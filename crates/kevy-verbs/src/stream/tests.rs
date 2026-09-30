use kevy_resp::Argv;
use kevy_store::{Store, StreamId};

use crate::{Effect, exec};

fn argv(cmd: &str) -> Argv {
    Argv::from(cmd.split(' ').map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>())
}

fn run(store: &mut Store, cmd: &str) -> (Option<Effect>, String) {
    let a = argv(cmd);
    let mut buf = [0u8; 32];
    let up = crate::args::upper_verb(&a[0], &mut buf).to_vec();
    let mut out = Vec::new();
    let e = exec(store, &up, &a, &mut out);
    (e, String::from_utf8_lossy(&out).into_owned())
}

fn frame(f: &[Vec<u8>]) -> String {
    f.iter().map(|p| String::from_utf8_lossy(p)).collect::<Vec<_>>().join(" ")
}

/// Apply one recorded frame the way a replay does: an internal record
/// frame by [`crate::aof::apply_internal`], anything else by `exec`.
fn replay(store: &mut Store, frame: &str) {
    let mut out = Vec::new();
    if !crate::aof::apply_internal(store, &argv(frame), &mut out) {
        run(store, frame);
    }
}

/// Every frame the effect of `cmd` records, as text; the argv itself for
/// a write. Built right after the command, as a recording caller does.
fn records(store: &mut Store, cmd: &str, e: Option<Effect>) -> Vec<String> {
    match e {
        Some(Effect::Write) => vec![cmd.to_string()],
        Some(Effect::Record(f)) => vec![frame(&f)],
        Some(
            e @ (Effect::RecordId(..)
            | Effect::RecordClaim(_)
            | Effect::RecordRead(..)
            | Effect::RecordReads(_)
            | Effect::RecordSeen),
        ) => {
            let frames = crate::aof::deferred_frames(&*store, &argv(cmd), &e);
            frames
                .iter()
                .map(|f| frame(&(0..f.len()).map(|i| f[i].to_vec()).collect::<Vec<_>>()))
                .collect()
        }
        _ => Vec::new(),
    }
}

#[test]
fn a_generated_id_is_recorded_as_the_id_it_gave() {
    let mut s = Store::new();
    let (e, reply) = run(&mut s, "XADD s NOMKSTREAM MAXLEN ~ 2 * f v");
    assert_eq!(e, Some(Effect::Unchanged), "{reply}");
    let (e, reply) = run(&mut s, "XADD s MAXLEN ~ 2 * f v");
    let id = reply.split("\r\n").nth(1).unwrap().to_string();
    assert_eq!(
        records(&mut s, "XADD s MAXLEN ~ 2 * f v", e),
        vec![format!("XADD s MAXLEN ~ 2 {id} f v")]
    );
    let (e, _) = run(&mut s, "XADD s2 7-* f v");
    assert_eq!(records(&mut s, "XADD s2 7-* f v", e), vec!["XADD s2 7-0 f v".to_string()]);
    let (e, _) = run(&mut s, "XADD s2 8-1 f v");
    assert_eq!(e, Some(Effect::Write), "an explicit ID is recorded as typed");
    let (e, reply) = run(&mut s, "XADD s2 1-* f v");
    assert!(reply.starts_with("-ERR The ID"), "{reply}");
    assert_eq!(e, Some(Effect::Write), "a refusal changes nothing, however it is recorded");
}

/// Replaying the records onto the state the commands started from gives
/// what the commands gave, whatever the clock says by then.
#[test]
fn claim_records_replay_to_the_same_pending_list() {
    let setup = ["XADD s 1-1 a 1", "XADD s 2-1 b 2", "XADD s 3-1 c 3", "XGROUP CREATE s g 0"];
    let mut live = Store::new();
    let mut log: Vec<String> = Vec::new();
    for c in setup.iter().chain(&["XREADGROUP GROUP g a STREAMS s >"]) {
        let (e, _) = run(&mut live, c);
        log.extend(records(&mut live, c, e));
    }
    let (_, gone) = run(&mut live, "XDEL s 3-1");
    assert_eq!(gone, ":1\r\n");
    log.push("XDEL s 3-1".into());
    // 4-1 was never delivered: only FORCE puts it in the list
    let (e, _) = run(&mut live, "XADD s 4-1 d 4");
    log.extend(records(&mut live, "XADD s 4-1 d 4", e));
    for c in [
        "XCLAIM s g b 0 1-1 2-1 RETRYCOUNT 7",
        "XAUTOCLAIM s g c 0 2-1 COUNT 5 JUSTID",
        "XCLAIM s g e 0 4-1 FORCE JUSTID",
    ] {
        let (e, _) = run(&mut live, c);
        let rec = records(&mut live, c, e);
        let ours = |f: &String| {
            f.starts_with("XCLAIM s g ") || f.starts_with("XINTERNAL.CONSUMERSEEN s g ")
        };
        assert!(!rec.is_empty() && rec.iter().all(ours), "{rec:?}");
        log.extend(rec);
    }
    // a replay runs later than the commands did
    std::thread::sleep(std::time::Duration::from_millis(5));
    let mut replayed = Store::new();
    for f in &log {
        replay(&mut replayed, f);
    }
    let read = "XPENDING s g - + 10";
    let blank = |s: String| {
        let mut t: Vec<String> = s.split("\r\n").map(str::to_string).collect();
        for i in 0..t.len() {
            if t[i] == "*4" && i + 5 < t.len() {
                t[i + 5] = ":idle".into();
            }
        }
        t.join("\r\n")
    };
    assert_eq!(blank(run(&mut replayed, read).1), blank(run(&mut live, read).1), "{log:#?}");
    // the idle column hides the delivery times; the rows hold them
    assert_eq!(pel(&mut replayed), pel(&mut live));
    assert_eq!(consumers(&replayed, b"s", b"g"), consumers(&live, b"s", b"g"));
    let pending = run(&mut live, read).1;
    assert!(pending.contains("$1\r\nb\r\n") && pending.contains("$1\r\nc\r\n"), "{pending}");
    assert!(pending.contains(":7\r\n"), "RETRYCOUNT survives: {pending}");
    assert!(pending.contains("$1\r\ne\r\n"), "FORCE created a row: {pending}");
}

/// `(id, owner, delivery time, delivery count)` of group `g` on `s`.
fn pel(store: &mut Store) -> Vec<(String, Vec<u8>, u64, u32)> {
    let s = store.stream_view(b"s").unwrap().unwrap();
    let g = s.group(b"g").unwrap();
    g.pending_range(..)
        .map(|(id, p)| {
            let owner = p.consumer.as_slice().to_vec();
            (String::from_utf8(id.encode()).unwrap(), owner, p.delivery_time_ms, p.delivery_count)
        })
        .collect()
}

/// `(consumer, last contact, pending)` of `group` on `key`, by name.
fn consumers(store: &Store, key: &[u8], group: &[u8]) -> Vec<(Vec<u8>, u64, usize)> {
    let g = store.stream_group_peek(key, group).expect("the group");
    let mut out: Vec<_> =
        g.consumers().map(|(n, c)| (n.to_vec(), c.last_seen_ms(), c.pending_count())).collect();
    out.sort();
    out
}

/// `(id, owner, delivery time, delivery count)`.
type PelRow = (StreamId, Vec<u8>, u64, u32);

/// The pending rows of `group` on `key`, and the group's last-delivered ID.
fn group_rows(store: &Store, key: &[u8], group: &[u8]) -> (Vec<PelRow>, StreamId) {
    let g = store.stream_group_peek(key, group).expect("the group");
    let rows = g
        .pending_range(..)
        .map(|(id, p)| (id, p.consumer.as_slice().to_vec(), p.delivery_time_ms, p.delivery_count))
        .collect();
    (rows, g.last_delivered_id())
}

/// One `XREADGROUP` over several streams is recorded stream by stream:
/// a stream it delivered from, a stream it delivered nothing from, and a
/// NOACK read of both each replay to where the read left them — including
/// when the stream that delivered is not the first one named.
#[test]
fn a_read_of_several_streams_replays_stream_by_stream() {
    let mut live = Store::new();
    let mut log: Vec<String> = Vec::new();
    for c in [
        "XADD a 1-1 x 1",
        "XADD a 2-1 x 2",
        "XADD b 1-1 y 1",
        "XGROUP CREATE a g 0",
        "XGROUP CREATE b g $",
        "XGROUP CREATE a n 0",
        "XGROUP CREATE b n 0",
        // a delivers two, b nothing
        "XREADGROUP GROUP g c1 STREAMS a b > >",
        // NOACK: both move, neither keeps a pending entry
        "XREADGROUP GROUP n c2 NOACK COUNT 1 STREAMS a b > >",
        "XADD b 2-1 y 2",
        // the first stream named delivers nothing, the second one entry
        "XREADGROUP GROUP g c3 STREAMS a b > >",
    ] {
        let (e, _) = run(&mut live, c);
        if c.starts_with("XREADGROUP") {
            assert!(matches!(e, Some(Effect::RecordReads(_))), "{c}: {e:?}");
        }
        log.extend(records(&mut live, c, e));
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    std::thread::sleep(std::time::Duration::from_millis(5));
    let mut replayed = Store::new();
    for f in &log {
        replay(&mut replayed, f);
    }
    for (key, group) in [(&b"a"[..], &b"g"[..]), (b"b", b"g"), (b"a", b"n"), (b"b", b"n")] {
        let want = group_rows(&live, key, group);
        assert_eq!(group_rows(&replayed, key, group), want, "{key:?} {group:?}: {log:#?}");
        let want = consumers(&live, key, group);
        assert_eq!(consumers(&replayed, key, group), want, "{key:?} {group:?}: {log:#?}");
    }
    let (b_rows, b_last) = group_rows(&live, b"b", b"g");
    assert_eq!((b_rows.len(), b_last), (1, StreamId::new(2, 1)), "b delivered 2-1 to c3");
    assert_eq!(b_rows[0].1, b"c3");
    let (a_rows, a_last) = group_rows(&live, b"a", b"n");
    assert_eq!((a_rows.len(), a_last), (0, StreamId::new(1, 1)), "NOACK moved a, kept nothing");
}

/// A server trims to `maxmemory` after a growing write and before it
/// records it, so the stream a claim just wrote can be gone by then:
/// nothing is left to state, and the record is empty rather than wrong.
#[test]
fn a_claim_whose_stream_is_gone_by_the_record_records_nothing() {
    let mut s = Store::new();
    for c in ["XADD s 1-1 a 1", "XGROUP CREATE s g 0", "XREADGROUP GROUP g a STREAMS s >"] {
        run(&mut s, c);
    }
    let claim = "XCLAIM s g a 0 1-1 JUSTID";
    let (e, reply) = run(&mut s, claim);
    assert!(matches!(e, Some(Effect::RecordClaim(_))), "{reply}");
    run(&mut s, "DEL s");
    let frames = crate::aof::deferred_frames(&s, &argv(claim), &e.expect("an effect"));
    assert!(frames.is_empty(), "{} frames for a stream that is gone", frames.len());
}

/// Group reads replayed from their records, later than they ran, leave
/// the pending list and the group where the reads left them: delivery
/// times included, an entry a NOACK read passed over left with its owner.
#[test]
fn read_records_replay_to_the_same_group() {
    let mut live = Store::new();
    let mut log: Vec<String> = Vec::new();
    for c in [
        "XADD s 1-1 a 1",
        "XADD s 2-1 b 2",
        "XADD s 3-1 c 3",
        "XGROUP CREATE s g 0",
        "XREADGROUP GROUP g c1 COUNT 1 STREAMS s >",
        "XREADGROUP GROUP g c2 STREAMS s >",
        "XGROUP SETID s g 0",
        // 1-1 again, past its owner c1: NOACK takes nothing into the list
        "XREADGROUP GROUP g c3 NOACK COUNT 1 STREAMS s >",
        "XREADGROUP GROUP g c4 STREAMS s 0",
        "XREADGROUP GROUP g c1 STREAMS s 0",
    ] {
        let (e, _) = run(&mut live, c);
        log.extend(records(&mut live, c, e));
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(!log.iter().any(|f| f.starts_with("XREADGROUP")), "{log:#?}");
    std::thread::sleep(std::time::Duration::from_millis(5));
    let mut replayed = Store::new();
    for f in &log {
        replay(&mut replayed, f);
    }
    assert_eq!(pel(&mut replayed), pel(&mut live), "{log:#?}");
    let owners: Vec<Vec<u8>> = pel(&mut live).into_iter().map(|r| r.1).collect();
    assert_eq!(owners, [b"c1".to_vec(), b"c2".to_vec(), b"c2".to_vec()]);
    for read in ["XINFO GROUPS s", "XINFO CONSUMERS s g"] {
        let (_, want) = run(&mut live, read);
        let (_, got) = run(&mut replayed, read);
        let blank = |s: &str| -> Vec<String> {
            s.split("\r\n").filter(|t| !t.starts_with(':')).map(str::to_string).collect()
        };
        assert_eq!(blank(&got), blank(&want), "{read}");
    }
}

/// A read that delivers nothing and makes no consumer is not recorded,
/// its contact with the group included: a consumer that only polls comes
/// back from a restart with the contact of its last recorded read. A read
/// that makes its consumer is recorded as that consumer's contact alone.
#[test]
fn an_empty_read_records_nothing() {
    let mut s = Store::new();
    for c in ["XADD s 1-1 a 1", "XGROUP CREATE s g 0", "XREADGROUP GROUP g a STREAMS s >"] {
        run(&mut s, c);
    }
    for poll in ["XREADGROUP GROUP g a STREAMS s >", "XREADGROUP GROUP g a STREAMS s 0"] {
        let (e, _) = run(&mut s, poll);
        assert_eq!(e, Some(Effect::Skip), "{poll}");
    }
    let history = "XREADGROUP GROUP g newbie STREAMS s 0";
    let (e, _) = run(&mut s, history);
    let rec = records(&mut s, history, e);
    assert_eq!(rec.len(), 1, "{rec:?}");
    assert!(rec[0].starts_with("XINTERNAL.CONSUMERSEEN s g newbie "), "{rec:?}");
}

/// A client cannot send the internal record verb through `exec`: it is
/// not a verb `exec` answers.
#[test]
fn the_internal_record_verb_is_not_a_client_verb() {
    let mut s = Store::new();
    run(&mut s, "XGROUP CREATE s g $ MKSTREAM");
    assert_eq!(run(&mut s, "XINTERNAL.CONSUMERSEEN s g c 40").0, None);
    let mut out = Vec::new();
    let malformed = argv("XINTERNAL.CONSUMERSEEN s g c soon");
    assert!(crate::aof::apply_internal(&mut s, &malformed, &mut out));
    assert!(out.starts_with(b"-ERR"), "{}", String::from_utf8_lossy(&out));
    assert!(consumers(&s, b"s", b"g").is_empty(), "a malformed record made a consumer");
}

#[test]
fn a_claim_that_changes_nothing_records_nothing() {
    let mut s = Store::new();
    for c in ["XADD s 1-1 a 1", "XGROUP CREATE s g 0", "XREADGROUP GROUP g a STREAMS s >"] {
        run(&mut s, c);
    }
    // the consumer is new: that much changed
    let (e, _) = run(&mut s, "XAUTOCLAIM s g idle 999999 0");
    let rec = records(&mut s, "XAUTOCLAIM s g idle 999999 0", e);
    assert_eq!(rec.len(), 1, "{rec:?}");
    assert!(rec[0].starts_with("XINTERNAL.CONSUMERSEEN s g idle "), "{rec:?}");
    let (e, _) = run(&mut s, "XAUTOCLAIM s g idle 999999 0");
    assert_eq!(e, Some(Effect::Skip));
    let (e, _) = run(&mut s, "XCLAIM s g a 999999 1-1");
    assert_eq!(e, Some(Effect::Skip));
    let (e, _) = run(&mut s, "XCLAIM s g a 0 9-9");
    assert_eq!(e, Some(Effect::Skip), "an ID nobody holds");
}

#[test]
fn a_dropped_entry_is_recorded_as_a_drop() {
    let mut s = Store::new();
    for c in ["XADD s 1-1 a 1", "XADD s 2-1 b 2", "XGROUP CREATE s g 0"] {
        run(&mut s, c);
    }
    run(&mut s, "XREADGROUP GROUP g a STREAMS s >");
    run(&mut s, "XDEL s 1-1");
    let (e, reply) = run(&mut s, "XCLAIM s g a 0 1-1 JUSTID");
    assert_eq!(reply, "*0\r\n");
    let rec = records(&mut s, "XCLAIM s g a 0 1-1 JUSTID", e);
    assert_eq!(rec, vec!["XCLAIM s g a 0 1-1 JUSTID".to_string()]);
}

/// A known consumer's read that delivers from one stream and not the
/// other records the stream it delivered from and nothing for the other.
#[test]
fn a_known_consumer_records_only_the_stream_that_delivered() {
    let mut s = Store::new();
    for c in ["XADD a 1-1 x 1", "XADD b 1-1 y 1", "XGROUP CREATE a g 0", "XGROUP CREATE b g 0"] {
        run(&mut s, c);
    }
    run(&mut s, "XREADGROUP GROUP g c STREAMS a b > >");
    run(&mut s, "XADD a 2-1 x 2");
    let read = "XREADGROUP GROUP g c STREAMS a b > >";
    let (e, _) = run(&mut s, read);
    let rec = records(&mut s, read, e);
    assert!(rec.iter().all(|f| !f.split(' ').any(|t| t == "b")), "{rec:?}");
    assert!(rec.iter().any(|f| f == "XGROUP SETID a g 2-1"), "{rec:?}");
}

/// An argv that is not a well-formed group read records nothing.
#[test]
fn a_read_record_of_an_argv_without_streams_is_empty() {
    let s = Store::new();
    let effect = Effect::RecordRead(StreamId::new(0, 0), crate::aof::Consumer::Created);
    for cmd in ["XREADGROUP GROUP g c COUNT 1 NOACK", "XREADGROUP GROUP g c STREAMS a b >"] {
        assert!(crate::aof::deferred_frames(&s, &argv(cmd), &effect).is_empty(), "{cmd}");
    }
}

#[test]
fn xpending_names_a_missing_group_and_a_key_that_is_not_a_stream_in_both_forms() {
    let mut s = Store::new();
    run(&mut s, "XADD s 1-1 f v");
    run(&mut s, "SET str v");
    for cmd in ["XPENDING s nog", "XPENDING s nog - + 10"] {
        assert_eq!(run(&mut s, cmd).1, "-NOGROUP No such consumer group\r\n", "{cmd}");
    }
    for cmd in ["XPENDING str g", "XPENDING str g - + 10"] {
        assert!(run(&mut s, cmd).1.starts_with("-WRONGTYPE"), "{cmd}");
    }
}

/// The names in an `XINFO` reply, in the order it lists them.
fn named(reply: &str) -> Vec<String> {
    let parts: Vec<&str> = reply.split("\r\n").collect();
    (1..parts.len().saturating_sub(2))
        .filter(|&i| parts[i] == "name")
        .map(|i| parts[i + 2].to_string())
        .collect()
}

/// Consumers and groups are listed by name in byte order, whatever order
/// they were made or read in. The expected replies are what a Redis 8.10
/// and a valkey 9.1 server answered for this script, byte for byte where
/// kevy's reply carries the same fields.
#[test]
fn consumers_and_groups_are_listed_by_name() {
    let mut s = Store::new();
    for c in [
        "XADD s 1-0 f a",
        "XADD s 2-0 f b",
        "XADD s 3-0 f c",
        "XADD s 4-0 f d",
        "XGROUP CREATE s g 0",
        "XREADGROUP GROUP g bob COUNT 1 STREAMS s >",
        "XREADGROUP GROUP g alice COUNT 1 STREAMS s >",
        "XREADGROUP GROUP g zed COUNT 1 STREAMS s >",
        "XREADGROUP GROUP g bob COUNT 1 STREAMS s >",
        "XGROUP CREATECONSUMER s g carol",
        "XGROUP CREATECONSUMER s g aaron",
        "XGROUP CREATE s g2 0",
        "XGROUP CREATE s a2 0",
    ] {
        run(&mut s, c);
    }
    assert_eq!(
        run(&mut s, "XPENDING s g").1,
        "*4\r\n:4\r\n$3\r\n1-0\r\n$3\r\n4-0\r\n*3\r\n\
         *2\r\n$5\r\nalice\r\n$1\r\n1\r\n\
         *2\r\n$3\r\nbob\r\n$1\r\n2\r\n\
         *2\r\n$3\r\nzed\r\n$1\r\n1\r\n"
    );
    assert_eq!(
        named(&run(&mut s, "XINFO CONSUMERS s g").1),
        ["aaron", "alice", "bob", "carol", "zed"]
    );
    assert_eq!(named(&run(&mut s, "XINFO GROUPS s").1), ["a2", "g", "g2"]);
}

/// The order is by bytes: not case-folded, not numeric, a prefix first.
#[test]
fn consumer_order_is_byte_order() {
    let mut s = Store::new();
    let names = ["zed", "ab", "Bob", "a", "alice", "b10", "b9"];
    for i in 1..=names.len() {
        run(&mut s, &format!("XADD s {i}-0 f v"));
    }
    run(&mut s, "XGROUP CREATE s g 0");
    for n in names {
        run(&mut s, &format!("XREADGROUP GROUP g {n} COUNT 1 STREAMS s >"));
        run(&mut s, &format!("XGROUP CREATECONSUMER s g {n}x"));
    }
    let summary = run(&mut s, "XPENDING s g").1;
    let listed: Vec<&str> =
        summary.split("\r\n").filter(|p| p.chars().any(char::is_alphabetic)).collect();
    assert_eq!(listed, ["Bob", "a", "ab", "alice", "b10", "b9", "zed"]);
    assert_eq!(
        named(&run(&mut s, "XINFO CONSUMERS s g").1),
        [
            "Bob", "Bobx", "a", "ab", "abx", "alice", "alicex", "ax", "b10", "b10x", "b9", "b9x",
            "zed", "zedx"
        ]
    );
}

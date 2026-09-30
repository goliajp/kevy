//! Streams and geo through the raw command channel: the verbs answer,
//! a blocking read is refused, and a stream with its consumer groups
//! survives both halves of host-mediated persistence (the appended
//! frames and the compacted image).
//!
//! The `unsafe` calls follow the note at the top of `abi_tests.rs`: live
//! handles, and pointer/length pairs taken from locals in scope.

use crate::abi_aof::{kevy_aof_dump, kevy_aof_frame_in, kevy_aof_frames_out};
use crate::abi_cmd::kevy_cmd;
use crate::abi_core::{OPEN_CAPTURE_AOF, kevy_close, kevy_open, kevy_out_len, kevy_out_ptr};
use crate::abi_kv::kevy_set;

fn out(h: u32) -> Vec<u8> {
    let ptr = kevy_out_ptr(h);
    // SAFETY: pointer and length are the pair the ABI just produced.
    unsafe { std::slice::from_raw_parts(ptr, kevy_out_len(h) as usize) }.to_vec()
}

fn cmd(h: u32, parts: &[&[u8]]) -> String {
    let mut packed = Vec::new();
    for p in parts {
        packed.extend_from_slice(&(p.len() as u32).to_le_bytes());
        packed.extend_from_slice(p);
    }
    // SAFETY: live handle, `packed` readable for the call.
    let s = unsafe { kevy_cmd(h, packed.as_ptr(), packed.len() as u32) };
    assert!(s >= 0, "{parts:?}: status {s}");
    String::from_utf8(out(h)).unwrap()
}

fn feed(h: u32, log: &[u8]) -> i32 {
    // SAFETY: live handle, `log` readable for the call.
    unsafe { kevy_aof_frame_in(h, log.as_ptr(), log.len() as u32) }
}

/// A stream with a group, one delivery acknowledged, one claimed by a
/// second consumer, a third consumer that has read nothing, and a geo set.
fn populate(h: u32) {
    for (id, v) in [("1-1", "a"), ("2-1", "b"), ("3-1", "c")] {
        assert_eq!(
            cmd(h, &[b"XADD", b"s", id.as_bytes(), b"f", v.as_bytes()]),
            format!("${}\r\n{id}\r\n", id.len())
        );
    }
    let auto = cmd(h, &[b"XADD", b"s", b"*", b"f", b"d"]);
    assert!(auto.starts_with('$'), "{auto}");
    assert_eq!(cmd(h, &[b"XGROUP", b"CREATE", b"s", b"g", b"0"]), "+OK\r\n");
    let read =
        cmd(h, &[b"XREADGROUP", b"GROUP", b"g", b"alice", b"COUNT", b"3", b"STREAMS", b"s", b">"]);
    assert!(read.contains("1-1") && read.contains("3-1") && !read.contains("$1\r\nd"), "{read}");
    assert_eq!(cmd(h, &[b"XACK", b"s", b"g", b"1-1"]), ":1\r\n");
    let claimed = cmd(h, &[b"XAUTOCLAIM", b"s", b"g", b"bob", b"0", b"0", b"COUNT", b"1"]);
    assert!(claimed.contains("2-1"), "{claimed}");
    assert_eq!(cmd(h, &[b"XGROUP", b"CREATECONSUMER", b"s", b"g", b"carol"]), ":1\r\n");
    let geo = cmd(
        h,
        &[
            b"GEOADD",
            b"g:city",
            b"139.6917",
            b"35.6895",
            b"tokyo",
            b"135.5023",
            b"34.6937",
            b"osaka",
        ],
    );
    assert_eq!(geo, ":2\r\n");
}

/// What a reload must reproduce: every reply here is independent of the
/// clock.
fn state(h: u32) -> Vec<String> {
    vec![
        cmd(h, &[b"XRANGE", b"s", b"-", b"+"]),
        cmd(h, &[b"XLEN", b"s"]),
        cmd(h, &[b"XPENDING", b"s", b"g"]),
        cmd(h, &[b"XINFO", b"CONSUMERS", b"s", b"g"]).lines().next().unwrap().to_owned(),
        cmd(h, &[b"GEODIST", b"g:city", b"tokyo", b"osaka", b"km"]),
        cmd(
            h,
            &[b"GEOSEARCH", b"g:city", b"FROMMEMBER", b"tokyo", b"BYRADIUS", b"500", b"km", b"ASC"],
        ),
        // every time FULL shows is absolute: the consumers' contact and
        // activity, the deliveries; and the group's read counter
        cmd(h, &[b"XINFO", b"STREAM", b"s", b"FULL"]),
    ]
}

#[test]
fn stream_and_geo_verbs_answer() {
    let h = kevy_open(0);
    populate(h);
    let s = state(h);
    assert_eq!(s[1], ":4\r\n");
    assert!(s[2].starts_with("*4\r\n:2\r\n$3\r\n2-1\r\n$3\r\n3-1\r\n"), "two pending: {}", s[2]);
    assert_eq!(s[3], "*3", "alice, bob and carol");
    let km: f64 = s[4].lines().nth(1).unwrap().parse().unwrap();
    assert!((390.0..410.0).contains(&km), "tokyo to osaka is about 400 km, got {km}");
    assert_eq!(s[5], "*2\r\n$5\r\ntokyo\r\n$5\r\nosaka\r\n");
    let read = cmd(h, &[b"XREAD", b"COUNT", b"1", b"STREAMS", b"s", b"2-1"]);
    assert!(read.contains("3-1") && !read.contains("2-1\r\n*"), "{read}");
    kevy_close(h);
}

#[test]
fn a_blocking_read_is_refused_and_says_why() {
    let h = kevy_open(0);
    cmd(h, &[b"XADD", b"s", b"1-1", b"f", b"v"]);
    cmd(h, &[b"XGROUP", b"CREATE", b"s", b"g", b"$"]);
    for argv in [
        &[&b"XREAD"[..], b"BLOCK", b"0", b"STREAMS", b"s", b"$"][..],
        &[b"XREAD", b"BLOCK", b"100", b"STREAMS", b"s", b"0"],
        &[b"XREADGROUP", b"GROUP", b"g", b"c", b"BLOCK", b"0", b"STREAMS", b"s", b">"],
    ] {
        let reply = cmd(h, argv);
        assert!(reply.starts_with("-ERR") && reply.contains("cannot block"), "{reply}");
    }
    assert!(cmd(h, &[b"BLPOP", b"l", b"0"]).starts_with("-ERR unknown command"));
    // the same read without BLOCK answers at once
    assert!(cmd(h, &[b"XREAD", b"STREAMS", b"s", b"0"]).contains("1-1"));
    kevy_close(h);
}

#[test]
fn cmd_writes_reach_the_pump_and_replay_to_the_same_state() {
    let src = kevy_open(OPEN_CAPTURE_AOF);
    // SAFETY: live handle, each pair readable for the call.
    unsafe { kevy_set(src, b"k".as_ptr(), 1, b"v".as_ptr(), 1) };
    populate(src);
    assert!(kevy_aof_frames_out(src) > 0);
    let log = out(src);
    let has = |needle: &[u8]| log.windows(needle.len()).any(|w| w == needle);
    assert!(has(b"XINTERNAL.CONSUMERSEEN"), "a new consumer is recorded by its internal frame");
    assert!(!has(b"\r\n*\r\n"), "XADD * is recorded with the id it chose");

    let dst = kevy_open(OPEN_CAPTURE_AOF);
    assert!(feed(dst, &log) > 0);
    assert_eq!(state(dst), state(src));
    assert_eq!(cmd(dst, &[b"GET", b"k"]), "$1\r\nv\r\n", "typed and cmd writes share one log");
    // the group's last delivered id came back: nothing new to read
    let again = cmd(
        dst,
        &[b"XREADGROUP", b"GROUP", b"g", b"alice", b"COUNT", b"1", b"STREAMS", b"s", b">"],
    );
    assert!(again.contains("$1\r\nd\r\n"), "only the fourth entry is new: {again}");
    kevy_close(src);
    kevy_close(dst);
}

#[test]
fn a_compacted_image_keeps_the_groups() {
    let src = kevy_open(OPEN_CAPTURE_AOF);
    populate(src);
    kevy_aof_dump(src);
    let image = out(src);
    let dst = kevy_open(0);
    assert!(feed(dst, &image) > 0);
    assert_eq!(state(dst), state(src));
    kevy_close(src);
    kevy_close(dst);
}

#[test]
fn a_read_through_cmd_records_nothing() {
    let h = kevy_open(OPEN_CAPTURE_AOF);
    populate(h);
    kevy_aof_frames_out(h);
    state(h);
    cmd(h, &[b"XREAD", b"STREAMS", b"s", b"0"]);
    assert_eq!(kevy_aof_frames_out(h), 0);
    kevy_close(h);
}

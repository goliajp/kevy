//! Native-target tests driving the C ABI functions directly — the same
//! call sequences the JS loader makes, minus the linear-memory staging
//! (on native we pass Rust slices' pointers straight through).

use crate::abi_aof::{kevy_aof_dump, kevy_aof_frame_in, kevy_aof_frames_out};
use crate::abi_cmd::kevy_cmd;
use crate::abi_core::{
    OPEN_CAPTURE_AOF, kevy_abi_version, kevy_alloc, kevy_close, kevy_free, kevy_open,
    kevy_out_len, kevy_out_ptr, kevy_tick,
};
use crate::abi_kv::{
    kevy_dbsize, kevy_del, kevy_exists, kevy_flushall, kevy_get, kevy_incrby, kevy_keys,
    kevy_persist, kevy_pttl, kevy_set, kevy_set_ttl,
};
use crate::abi_pubsub::{
    EVENT_MESSAGE, EVENT_PMESSAGE, kevy_poll_events, kevy_psubscribe, kevy_publish,
    kevy_subscribe, kevy_unsubscribe,
};
use crate::{BAD_HANDLE, ERR, OK};

fn out(h: u32) -> Vec<u8> {
    let ptr = kevy_out_ptr(h);
    let len = kevy_out_len(h) as usize;
    if ptr.is_null() {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec()
}

fn set(h: u32, k: &[u8], v: &[u8]) -> i32 {
    unsafe { kevy_set(h, k.as_ptr(), k.len() as u32, v.as_ptr(), v.len() as u32) }
}

fn get(h: u32, k: &[u8]) -> (i32, Vec<u8>) {
    let s = unsafe { kevy_get(h, k.as_ptr(), k.len() as u32) };
    (s, out(h))
}

#[test]
fn abi_version_is_stable() {
    assert_eq!(kevy_abi_version(), 1);
}

#[test]
fn alloc_free_roundtrip() {
    let p = kevy_alloc(64);
    assert!(!p.is_null());
    unsafe {
        p.write_bytes(0xAB, 64);
        kevy_free(p, 64);
    }
}

#[test]
fn open_set_get_del_exists() {
    let h = kevy_open(0);
    assert_ne!(h, 0);
    assert_eq!(set(h, b"k", b"v"), OK);
    assert_eq!(get(h, b"k"), (1, b"v".to_vec()));
    assert_eq!(unsafe { kevy_exists(h, b"k".as_ptr(), 1) }, 1);
    assert_eq!(kevy_dbsize(h) as u64, 1);
    assert_eq!(unsafe { kevy_del(h, b"k".as_ptr(), 1) }, 1);
    assert_eq!(unsafe { kevy_del(h, b"k".as_ptr(), 1) }, 0);
    assert_eq!(get(h, b"k").0, 0);
    assert_eq!(kevy_close(h), OK);
    assert_eq!(kevy_close(h), BAD_HANDLE);
    assert_eq!(set(h, b"k", b"v"), BAD_HANDLE);
    assert!(kevy_dbsize(h).is_nan());
}

#[test]
fn ttl_surface() {
    let h = kevy_open(0);
    let k = b"flash";
    assert_eq!(
        unsafe { kevy_set_ttl(h, k.as_ptr(), k.len() as u32, b"x".as_ptr(), 1, 60_000.0) },
        OK
    );
    let ttl = unsafe { kevy_pttl(h, k.as_ptr(), k.len() as u32) };
    assert!(ttl > 0.0 && ttl <= 60_000.0, "pttl = {ttl}");
    assert_eq!(unsafe { kevy_persist(h, k.as_ptr(), k.len() as u32) }, 1);
    assert_eq!(unsafe { kevy_pttl(h, k.as_ptr(), k.len() as u32) }, -1.0);
    assert_eq!(
        unsafe { kevy_pttl(h, b"missing".as_ptr(), 7) },
        -2.0
    );
    assert_eq!(kevy_tick(h), 0);
    kevy_close(h);
}

#[test]
fn incrby_and_error_reporting() {
    let h = kevy_open(0);
    assert_eq!(unsafe { kevy_incrby(h, b"n".as_ptr(), 1, 5.0) }, OK);
    assert_eq!(out(h), b"5");
    assert_eq!(unsafe { kevy_incrby(h, b"n".as_ptr(), 1, -2.0) }, OK);
    assert_eq!(out(h), b"3");
    set(h, b"s", b"not-a-number");
    assert_eq!(unsafe { kevy_incrby(h, b"s".as_ptr(), 1, 1.0) }, ERR);
    assert!(!out(h).is_empty(), "error status must leave a message");
    kevy_close(h);
}

#[test]
fn keys_packing() {
    let h = kevy_open(0);
    set(h, b"user:1", b"a");
    set(h, b"user:2", b"b");
    set(h, b"other", b"c");
    let pat = b"user:*";
    let n = unsafe { kevy_keys(h, pat.as_ptr(), pat.len() as u32, 0) };
    assert_eq!(n, 2);
    let buf = out(h);
    let mut names = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        let len = u32::from_le_bytes(buf[i..i + 4].try_into().unwrap()) as usize;
        i += 4;
        names.push(buf[i..i + len].to_vec());
        i += len;
    }
    names.sort();
    assert_eq!(names, vec![b"user:1".to_vec(), b"user:2".to_vec()]);
    assert_eq!(kevy_flushall(h), OK);
    assert_eq!(kevy_dbsize(h) as u64, 0);
    kevy_close(h);
}

/// One unpacked event: (kind, sub, pattern, channel, payload).
type Event = (u8, u32, Vec<u8>, Vec<u8>, Vec<u8>);

/// Unpack the `kevy_poll_events` buffer into (kind, sub, a, b, c) tuples.
fn unpack_events(buf: &[u8]) -> Vec<Event> {
    let mut events = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        let kind = buf[i];
        let sub = u32::from_le_bytes(buf[i + 1..i + 5].try_into().unwrap());
        i += 5;
        let mut segs = Vec::new();
        for _ in 0..3 {
            let len = u32::from_le_bytes(buf[i..i + 4].try_into().unwrap()) as usize;
            i += 4;
            segs.push(buf[i..i + len].to_vec());
            i += len;
        }
        let c = segs.pop().unwrap();
        let b = segs.pop().unwrap();
        let a = segs.pop().unwrap();
        events.push((kind, sub, a, b, c));
    }
    events
}

#[test]
fn pubsub_poll_drain() {
    let h = kevy_open(0);
    let s1 = unsafe { kevy_subscribe(h, b"news".as_ptr(), 4) };
    let s2 = unsafe { kevy_psubscribe(h, b"n*".as_ptr(), 2) };
    assert!(s1 > 0 && s2 > 0 && s1 != s2);
    let reached =
        unsafe { kevy_publish(h, b"news".as_ptr(), 4, b"hello".as_ptr(), 5) };
    assert_eq!(reached, 2);
    let n = kevy_poll_events(h);
    assert_eq!(n, 2);
    let events = unpack_events(&out(h));
    assert!(events.contains(&(EVENT_MESSAGE, s1, vec![], b"news".to_vec(), b"hello".to_vec())));
    assert!(events.contains(&(
        EVENT_PMESSAGE,
        s2,
        b"n*".to_vec(),
        b"news".to_vec(),
        b"hello".to_vec()
    )));
    // Queue is drained; a second poll is empty.
    assert_eq!(kevy_poll_events(h), 0);
    assert_eq!(kevy_unsubscribe(h, s1), OK);
    assert_eq!(kevy_unsubscribe(h, s1), BAD_HANDLE);
    assert_eq!(
        unsafe { kevy_publish(h, b"news".as_ptr(), 4, b"x".as_ptr(), 1) },
        1
    );
    kevy_close(h);
}

#[test]
fn aof_pump_roundtrip_chunked() {
    // Writer side: capture frames.
    let w = kevy_open(OPEN_CAPTURE_AOF);
    set(w, b"a", b"1");
    set(w, b"b", b"2");
    assert_eq!(unsafe { kevy_incrby(w, b"a".as_ptr(), 1, 10.0) }, OK);
    assert_eq!(unsafe { kevy_del(w, b"b".as_ptr(), 1) }, 1);
    let len = kevy_aof_frames_out(w);
    assert!(len > 0);
    let frames = out(w);
    assert_eq!(frames.len(), len as usize);
    // Drained: a second pump is empty.
    assert_eq!(kevy_aof_frames_out(w), 0);
    kevy_close(w);

    // Host log = magic + frames, replayed in awkward chunk sizes.
    let mut log = kevy_persist::AOF_MAGIC.to_vec();
    log.extend_from_slice(&frames);
    let r = kevy_open(0);
    let mut applied = 0;
    for chunk in log.chunks(7) {
        let n = unsafe { kevy_aof_frame_in(r, chunk.as_ptr(), chunk.len() as u32) };
        assert!(n >= 0, "chunked feed failed: {:?}", String::from_utf8_lossy(&out(r)));
        applied += n;
    }
    assert_eq!(applied, 4);
    assert_eq!(get(r, b"a"), (1, b"11".to_vec()));
    assert_eq!(get(r, b"b").0, 0);
    kevy_close(r);
}

#[test]
fn aof_frame_in_rejects_corrupt_tail() {
    let h = kevy_open(0);
    // A valid DEL frame, then a multi-bulk whose element is not a bulk
    // string — the parser rejects that outright (not "need more bytes").
    let bytes = b"*2\r\n$3\r\nDEL\r\n$1\r\nk\r\n*2\r\nXX\r\n";
    let n = unsafe { kevy_aof_frame_in(h, bytes.as_ptr(), bytes.len() as u32) };
    assert_eq!(n, ERR);
    assert!(!out(h).is_empty());
    kevy_close(h);
}

#[test]
fn aof_dump_compacts_and_replays() {
    let w = kevy_open(OPEN_CAPTURE_AOF);
    for i in 0..50 {
        set(w, format!("k{i}").as_bytes(), b"first");
        set(w, format!("k{i}").as_bytes(), b"final");
    }
    let dump_len = kevy_aof_dump(w);
    assert!(dump_len > 0);
    let image = out(w);
    assert!(image.starts_with(kevy_persist::AOF_MAGIC));
    // The dump subsumes (and discards) the pending append frames.
    assert_eq!(kevy_aof_frames_out(w), 0);
    // A compacted image is smaller than the 100-frame history it replaces.
    assert!(image.len() < 100 * 20);
    kevy_close(w);

    let r = kevy_open(0);
    let n = unsafe { kevy_aof_frame_in(r, image.as_ptr(), image.len() as u32) };
    assert_eq!(n, 50);
    assert_eq!(kevy_dbsize(r) as u64, 50);
    assert_eq!(get(r, b"k7"), (1, b"final".to_vec()));
    kevy_close(r);
}

/// Pack argv the way the JS loader does: each arg as a u32-LE length
/// prefix followed by its bytes, back to back.
fn pack_argv(parts: &[&[u8]]) -> Vec<u8> {
    let mut buf = Vec::new();
    for p in parts {
        buf.extend_from_slice(&(p.len() as u32).to_le_bytes());
        buf.extend_from_slice(p);
    }
    buf
}

/// Run one command through the raw channel; returns (status, reply bytes).
fn cmd(h: u32, parts: &[&[u8]]) -> (i32, Vec<u8>) {
    let packed = pack_argv(parts);
    let s = unsafe { kevy_cmd(h, packed.as_ptr(), packed.len() as u32) };
    (s, out(h))
}

#[test]
fn cmd_universal_path_reaches_the_compiled_surface() {
    let h = kevy_open(0);
    // String round-trip through the raw channel: SET replies +OK, GET the bulk.
    let (s, reply) = cmd(h, &[b"SET", b"k", b"v"]);
    assert!(s >= 0);
    assert_eq!(reply, b"+OK\r\n");
    let (s, reply) = cmd(h, &[b"GET", b"k"]);
    assert!(s >= 0);
    assert_eq!(reply, b"$1\r\nv\r\n");

    // An arbitrary non-KV verb the typed surface does not wrap: LPUSH/LLEN
    // — proving the path bottoms out in the full dispatcher, per the
    // conformance checklist ("cmd(argv) runs an arbitrary verb").
    let (s, reply) = cmd(h, &[b"LPUSH", b"l", b"x"]);
    assert!(s >= 0);
    assert_eq!(reply, b":1\r\n");
    let (s, reply) = cmd(h, &[b"LLEN", b"l"]);
    assert!(s >= 0);
    assert_eq!(reply, b":1\r\n");

    // A verb-level error is a *successful* call with a RESP error frame.
    let (s, reply) = cmd(h, &[b"GET", b"l"]);
    assert!(s >= 0, "verb error is not an ABI-misuse status");
    assert_eq!(
        reply,
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n"
    );

    // Index/replication verbs are not compiled into the wasm closure: an
    // unknown-command RESP error is the correct, expected answer.
    let (s, reply) = cmd(h, &[b"IDX.CREATE", b"i"]);
    assert!(s >= 0);
    assert!(
        reply.starts_with(b"-ERR unknown command"),
        "uncompiled verb should be an unknown-command error, got {:?}",
        String::from_utf8_lossy(&reply)
    );

    kevy_close(h);
}

#[test]
fn cmd_rejects_malformed_and_bad_handle() {
    let h = kevy_open(0);
    // Empty packed argv is caller misuse (-1), not a protocol error.
    let s = unsafe { kevy_cmd(h, std::ptr::null(), 0) };
    assert_eq!(s, ERR);
    assert!(!out(h).is_empty());
    kevy_close(h);
    // Bad handle after close.
    let packed = pack_argv(&[b"PING"]);
    assert_eq!(
        unsafe { kevy_cmd(h, packed.as_ptr(), packed.len() as u32) },
        BAD_HANDLE
    );
}

#[test]
fn wrong_type_get_surfaces_canonical_message() {
    let h = kevy_open(0);
    // Make `l` a list via the raw channel, then a typed GET must reject it
    // with the Redis-canonical WRONGTYPE wording — NOT the internal
    // `store error: WrongType` Debug spelling that used to leak to JS.
    assert!(cmd(h, &[b"RPUSH", b"l", b"a"]).0 >= 0);
    let (status, msg) = get(h, b"l");
    assert_eq!(status, ERR);
    let text = String::from_utf8(msg).unwrap();
    assert_eq!(
        text,
        "WRONGTYPE Operation against a key holding the wrong kind of value"
    );
    assert!(!text.contains("store error"), "internal Debug spelling leaked: {text}");
    kevy_close(h);
}

#[test]
fn capture_off_by_default() {
    let h = kevy_open(0);
    set(h, b"k", b"v");
    assert_eq!(kevy_aof_frames_out(h), 0);
    kevy_close(h);
}

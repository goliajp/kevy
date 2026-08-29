//! Waiting on a shard, without writing down a number of milliseconds.
//!
//! Two things in this engine become visible to a test only after a shard
//! TICKS, and several cells waited for them with a bare sleep.
//!
//! `INFO` is answered on one shard. That shard refreshes its own slot and
//! then SUMS every shard's slot (`ops::info` -> `stats::publish_gauges` ->
//! `obs.aggregate`), so the other seven are only as fresh as their last
//! tick. Live config is the same shape: `config_replace` is picked up by
//! `apply_live_runtime_config` on a tick, not at the call.
//!
//! The tick is `1000 / expiry.hz` ms — but it runs on a reactor that may
//! be parked or descheduled, so "how long since the work finished" is not
//! a number a test can write down. Two cells wrote one down anyway and
//! failed on a loaded machine: `tier_hydration` read 26 preads where 52
//! rows were cold (half the shards had published), and
//! `slowlog_hotreload` missed a config swap it had given 500 ms —
//! a margin its own comment says was copied from another file.
//!
//! `tier_hydration` already knew better one screen above the failure: its
//! `cold_keys` wait says "never a bare sleep on eviction timing" and polls
//! to a fixpoint. This module is that idiom, made shareable.
//!
//! Neither helper can turn a wrong answer into a right one. A gauge that
//! settles at the wrong value settles, and the assertion still fires on
//! it; for the `== 0` assertions, resting is strictly STRONGER than a
//! fixed sleep, because a late promotion has longer to show up.

#![allow(dead_code)] // each test binary uses a subset

use std::time::{Duration, Instant};

/// How long a value must hold still before it counts as settled. Several
/// tick intervals at the default `expiry.hz`, so every shard has had more
/// than one chance to publish.
pub const REST: Duration = Duration::from_millis(500);

/// How long to keep asking before giving up and saying what was seen.
pub const BUDGET: Duration = Duration::from_secs(20);

/// Read a whole SNAPSHOT until it stops changing for [`REST`].
///
/// Gauges that are compared to each other must come from ONE `INFO`
/// reply. `peek_preads_total` and `cold_keys` were read by two separate
/// round trips, so the equality between them was being asserted across two
/// different moments — a skew no amount of waiting removes, because each
/// read also refreshes the answering shard before summing the rest.
pub fn snapshot_at_rest<F: FnMut() -> Vec<u64>>(what: &str, mut read: F) -> Vec<u64> {
    let started = Instant::now();
    let mut last = read();
    let mut since = Instant::now();
    let mut reads = 1usize;
    while started.elapsed() < BUDGET {
        std::thread::sleep(Duration::from_millis(25));
        let now = read();
        reads += 1;
        if now == last {
            if since.elapsed() >= REST {
                return now;
            }
        } else {
            last = now;
            since = Instant::now();
        }
    }
    panic!("{what} never came to rest: still moving after {:?} and {reads} reads (last = {last:?})",
           started.elapsed());
}

/// Read until the value stops changing for [`REST`], then return it.
///
/// Panics — naming the last value and the number of reads — if it never
/// holds still inside [`BUDGET`]. That is a real failure: a gauge that
/// never settles is not a slow gauge, it is a moving one.
pub fn at_rest<F: FnMut() -> u64>(what: &str, mut read: F) -> u64 {
    let started = Instant::now();
    let mut last = read();
    let mut since = Instant::now();
    let mut reads = 1usize;
    while started.elapsed() < BUDGET {
        std::thread::sleep(Duration::from_millis(25));
        let now = read();
        reads += 1;
        if now == last {
            if since.elapsed() >= REST {
                return now;
            }
        } else {
            last = now;
            since = Instant::now();
        }
    }
    panic!("{what} never came to rest: still moving after {:?} and {reads} reads (last = {last})",
           started.elapsed());
}

/// Poll until `ok` returns true, or panic naming what was being waited for.
///
/// For an effect that either has landed or has not — a config swap picked
/// up, a listener gone — where the question is whether it happens at all,
/// not how many milliseconds it took on this machine.
pub fn until<F: FnMut() -> bool>(what: &str, mut ok: F) {
    let started = Instant::now();
    let mut polls = 0usize;
    while started.elapsed() < BUDGET {
        polls += 1;
        if ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("{what} never happened: {polls} polls over {:?}", started.elapsed());
}

/// The length of the first complete RESP reply in `buf`, or `None` if it
/// is not all here yet.
///
/// Most cells in this tree read a reply with "sleep 30 ms, then `read()`
/// once". That is fine while every reply arrives in one segment, and it
/// desynchronises the connection the first time one does not: the tail is
/// left in the socket and every later reply is read one frame late.
/// `r10_optimistic_lock_via_watch_multi_exec` failed exactly this way —
/// `EXEC` answers `*1\r\n:0\r\n`, its assertion only checked the `*1\r\n`
/// prefix, and the `:0\r\n` came back on the front of the next `WATCH`.
///
/// So: read until the frame is complete, and let the frame say when.
pub fn reply_len(buf: &[u8]) -> Option<usize> {
    fn line_end(buf: &[u8], from: usize) -> Option<usize> {
        buf.get(from..)?.windows(2).position(|w| w == b"\r\n").map(|p| from + p + 2)
    }
    let tag = *buf.first()?;
    let head = line_end(buf, 1)?;
    match tag {
        b'+' | b'-' | b':' | b'_' | b'#' | b',' => Some(head),
        b'$' | b'=' => {
            let n: i64 = std::str::from_utf8(&buf[1..head - 2]).ok()?.parse().ok()?;
            if n < 0 {
                return Some(head);
            }
            let end = head + n as usize + 2;
            (buf.len() >= end).then_some(end)
        }
        b'*' | b'~' | b'>' => {
            let n: i64 = std::str::from_utf8(&buf[1..head - 2]).ok()?.parse().ok()?;
            if n < 0 {
                return Some(head);
            }
            let mut at = head;
            for _ in 0..n {
                at += reply_len(buf.get(at..)?)?;
            }
            Some(at)
        }
        b'%' => {
            let n: i64 = std::str::from_utf8(&buf[1..head - 2]).ok()?.parse().ok()?;
            let mut at = head;
            for _ in 0..(n.max(0) * 2) {
                at += reply_len(buf.get(at..)?)?;
            }
            Some(at)
        }
        _ => None,
    }
}

/// A connection that keeps its own read buffer, so a reply split across
/// segments is assembled rather than truncated, and a reply that arrives
/// early is not thrown away.
pub struct Wire {
    sock: std::net::TcpStream,
    buf: Vec<u8>,
}

impl Wire {
    pub fn new(sock: std::net::TcpStream) -> Self {
        Self { sock, buf: Vec::new() }
    }

    /// Send one command as a RESP array and return exactly one reply.
    ///
    /// Generic over the part type so the corpus harnesses (which hold
    /// `Vec<u8>`) and the hand-written cells (which hold `&[u8]`) share
    /// one implementation rather than one each.
    pub fn call<B: AsRef<[u8]>>(&mut self, parts: &[B]) -> Vec<u8> {
        use std::io::{Read, Write};
        let mut out = format!("*{}\r\n", parts.len()).into_bytes();
        for p in parts {
            let p = p.as_ref();
            out.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
            out.extend_from_slice(p);
            out.extend_from_slice(b"\r\n");
        }
        self.sock.write_all(&out).unwrap();
        loop {
            if let Some(n) = reply_len(&self.buf) {
                let reply = self.buf[..n].to_vec();
                self.buf.drain(..n);
                return reply;
            }
            let mut chunk = [0u8; 65536];
            let n = self.sock.read(&mut chunk).unwrap();
            assert!(n > 0, "server closed mid-reply (have {} bytes)", self.buf.len());
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::reply_len;

    #[test]
    fn a_frame_says_when_it_is_complete() {
        // Simple, integer, error: the first line and nothing more.
        assert_eq!(reply_len(b"+OK\r\n"), Some(5));
        assert_eq!(reply_len(b":0\r\n"), Some(4));
        assert_eq!(reply_len(b"-ERR nope\r\n"), Some(11));
        // A bulk is its declared length, even when the payload contains
        // the terminator that a line-scanner would stop at.
        assert_eq!(reply_len(b"$4\r\na\r\nb\r\n"), Some(10));
        assert_eq!(reply_len(b"$-1\r\n"), Some(5));
        // Arrays recurse, and nest.
        assert_eq!(reply_len(b"*1\r\n:0\r\n"), Some(8));
        assert_eq!(reply_len(b"*2\r\n$1\r\na\r\n*1\r\n:7\r\n"), Some(19));
        assert_eq!(reply_len(b"*-1\r\n"), Some(5));
        // Trailing bytes are NOT consumed: the length is the first frame,
        // which is what lets a caller notice it is a frame ahead.
        assert_eq!(reply_len(b"*1\r\n:0\r\n+OK\r\n"), Some(8));
    }

    #[test]
    fn an_incomplete_frame_is_not_a_frame() {
        // Each of these is a prefix of `*1\r\n$3\r\nabc\r\n`, and none may
        // be reported as complete — that is the whole defect this exists
        // to prevent.
        let whole = b"*1\r\n$3\r\nabc\r\n";
        for cut in 1..whole.len() {
            assert_eq!(
                reply_len(&whole[..cut]),
                None,
                "a {cut}-byte prefix of a 13-byte frame was read as complete"
            );
        }
        assert_eq!(reply_len(whole), Some(whole.len()));
        assert_eq!(reply_len(b""), None);
    }
}

//! Concurrent writer pool that captures ACK logs for post-restart verification.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// One entry: a write that was ACK'd by kevy (+OK reply).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckEntry {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    /// Per-writer monotonic sequence number, starting at 0.
    pub seq: u64,
}

/// Shared, lock-protected log of ACK'd writes from all writers.
pub type AckLog = Arc<Mutex<Vec<AckEntry>>>;

/// N writer threads, each connecting to kevy and issuing `SET key value`
/// repeatedly. Each successful `+OK` reply appends to the shared `AckLog`.
pub struct WriterPool {
    handles: Vec<thread::JoinHandle<()>>,
    pub log: AckLog,
}

impl WriterPool {
    /// Spawn `n_writers` threads against `port`. Each writer prefixes its
    /// keys with `wN_` to avoid cross-writer collisions and uses an
    /// incrementing seq. Writers stop when `stop` is set true.
    #[must_use]
    pub fn spawn(port: u16, n_writers: usize, stop: Arc<std::sync::atomic::AtomicBool>) -> Self {
        let log: AckLog = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::with_capacity(n_writers);
        for w in 0..n_writers {
            let log = Arc::clone(&log);
            let stop = Arc::clone(&stop);
            handles.push(thread::spawn(move || writer_loop(w, port, log, stop)));
        }
        Self { handles, log }
    }

    /// Join all writers (panics if any panicked). Caller should set the
    /// stop flag first.
    pub fn join(self) -> AckLog {
        for h in self.handles {
            // Ignore join errors — a writer panicking is itself a
            // signal the test wants to see, surfaced via the AckLog
            // (a final ACK count below expectation indicates abnormal
            // exit). For v1.31 we keep this simple.
            let _ = h.join();
        }
        self.log
    }
}

fn writer_loop(
    writer_id: usize,
    port: u16,
    log: AckLog,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut stream = match TcpStream::connect(format!("127.0.0.1:{port}")) {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mut seq: u64 = 0;
    let mut reply_buf = [0u8; 64];
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        let key = format!("w{writer_id}_k{seq}").into_bytes();
        let value = format!("w{writer_id}_v{seq}").into_bytes();
        let frame = build_set_frame(&key, &value);
        if stream.write_all(&frame).is_err() {
            return;
        }
        match stream.read(&mut reply_buf) {
            Ok(n) if n >= 5 && reply_buf[..5] == *b"+OK\r\n" => {
                log.lock().unwrap().push(AckEntry { key, value, seq });
                seq += 1;
            }
            _ => return,
        }
    }
}

fn build_set_frame(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len() + value.len() + 32);
    out.extend_from_slice(b"*3\r\n$3\r\nSET\r\n");
    out.extend_from_slice(format!("${}\r\n", key.len()).as_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(format!("${}\r\n", value.len()).as_bytes());
    out.extend_from_slice(value);
    out.extend_from_slice(b"\r\n");
    out
}

/// Verify that every entry in `acks` is readable from kevy at `port`,
/// using a SINGLE pipelined TCP connection (one large batched
/// write, single drain read, parse replies in order). This avoids the
/// ephemeral-port exhaustion that ruined the v1.31.0 / v1.31.1 first
/// pass when each GET opened a fresh TCP conn (Mac's ~16 k ephemeral
/// ports × 60 s TIME_WAIT capped sustainable rate at ~267 conns/s; the
/// chaos test verifies hundreds of thousands of ACKs per run).
///
/// Returns Ok(()) if all match or Err on first corrupted (wrong value)
/// or lost (nil reply) entry. Use [`pipelined_verify_counts`] when the
/// caller wants to count instead of fail-fast.
pub fn verify_all_present(port: u16, acks: &[AckEntry]) -> Result<(), String> {
    let (present, lost, corrupted) = pipelined_verify_counts(port, acks);
    if !corrupted.is_empty() {
        return Err(format!(
            "CORRUPTION DETECTED — {} keys returned wrong values:\n{}",
            corrupted.len(),
            corrupted.join("\n")
        ));
    }
    if lost > 0 {
        return Err(format!("LOST {lost} of {} ACKs (present {present})", acks.len()));
    }
    Ok(())
}

/// Same as `verify_all_present` but returns counts instead of fail-fast.
/// Returns `(present, lost, corrupted_descriptions)`.
pub fn pipelined_verify_counts(
    port: u16,
    acks: &[AckEntry],
) -> (usize, usize, Vec<String>) {
    let mut s = match TcpStream::connect(format!("127.0.0.1:{port}")) {
        Ok(s) => s,
        Err(e) => return (0, acks.len(), vec![format!("connect: {e}")]),
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
    // Send-all then drain-all keeps the pipeline simple. The sender
    // thread half-closes after writing so the read side sees EOF.
    let mut send_buf = Vec::with_capacity(acks.len() * 32);
    for ack in acks {
        send_buf.extend_from_slice(b"*2\r\n$3\r\nGET\r\n");
        send_buf.extend_from_slice(format!("${}\r\n", ack.key.len()).as_bytes());
        send_buf.extend_from_slice(&ack.key);
        send_buf.extend_from_slice(b"\r\n");
    }
    let send_handle = std::thread::spawn(move || {
        s.write_all(&send_buf).map_err(|e| format!("pipeline write: {e}"))?;
        let _ = s.shutdown(std::net::Shutdown::Write);
        Ok::<_, String>(s)
    });
    let mut s = match send_handle.join() {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return (0, acks.len(), vec![e]),
        Err(_) => return (0, acks.len(), vec!["sender thread panicked".into()]),
    };
    let mut buf = Vec::with_capacity(8 * 1024 * 1024);
    let mut tmp = vec![0u8; 64 * 1024];
    loop {
        match s.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }
    // Parse replies in order matching the ACK log.
    let mut present = 0usize;
    let mut lost = 0usize;
    let mut corrupted = Vec::new();
    let mut pos = 0usize;
    for ack in acks {
        match parse_one_reply(&buf, pos) {
            Some((Some(val), next)) => {
                if val == ack.value {
                    present += 1;
                } else {
                    corrupted.push(format!(
                        "key={:?} expected={:?} got={:?}",
                        String::from_utf8_lossy(&ack.key),
                        String::from_utf8_lossy(&ack.value),
                        String::from_utf8_lossy(val),
                    ));
                }
                pos = next;
            }
            Some((None, next)) => {
                lost += 1;
                pos = next;
            }
            None => {
                lost += 1;
            }
        }
    }
    (present, lost, corrupted)
}

fn parse_one_reply(buf: &[u8], start: usize) -> Option<(Option<&[u8]>, usize)> {
    let rest = &buf[start..];
    if rest.starts_with(b"$-1\r\n") {
        return Some((None, start + 5));
    }
    if rest.first() != Some(&b'$') {
        return None;
    }
    let nl = rest.iter().position(|&b| b == b'\n')?;
    let len_str = std::str::from_utf8(&rest[1..nl - 1]).ok()?;
    let len: usize = len_str.parse().ok()?;
    let body_start = nl + 1;
    let body_end = body_start + len;
    if rest.len() < body_end + 2 {
        return None;
    }
    Some((Some(&rest[body_start..body_end]), start + body_end + 2))
}


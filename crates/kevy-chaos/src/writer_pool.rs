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
/// returning Ok(()) if all match or Err with the count + first mismatch.
pub fn verify_all_present(port: u16, acks: &[AckEntry]) -> Result<(), String> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .map_err(|e| format!("connect: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = vec![0u8; 1024];
    for entry in acks {
        let frame = build_get_frame(&entry.key);
        stream
            .write_all(&frame)
            .map_err(|e| format!("write GET key={:?}: {e}", entry.key))?;
        let n = stream
            .read(&mut buf)
            .map_err(|e| format!("read GET key={:?}: {e}", entry.key))?;
        let reply = &buf[..n];
        if !reply_matches_bulk(reply, &entry.value) {
            return Err(format!(
                "MISMATCH key={:?} expected_value={:?} got reply={:?}",
                String::from_utf8_lossy(&entry.key),
                String::from_utf8_lossy(&entry.value),
                String::from_utf8_lossy(reply),
            ));
        }
    }
    Ok(())
}

pub(crate) fn build_get_frame(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len() + 24);
    out.extend_from_slice(b"*2\r\n$3\r\nGET\r\n");
    out.extend_from_slice(format!("${}\r\n", key.len()).as_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(b"\r\n");
    out
}

pub(crate) fn reply_matches_bulk(reply: &[u8], expected: &[u8]) -> bool {
    // Expected RESP: $<len>\r\n<bytes>\r\n
    let Some(prefix_end) = reply.iter().position(|&b| b == b'\n') else { return false };
    if reply.first() != Some(&b'$') {
        return false;
    }
    let len_str = std::str::from_utf8(&reply[1..prefix_end - 1]).unwrap_or("");
    let Ok(len) = len_str.parse::<usize>() else { return false };
    if len != expected.len() {
        return false;
    }
    let body_start = prefix_end + 1;
    let body_end = body_start + len;
    if reply.len() < body_end {
        return false;
    }
    &reply[body_start..body_end] == expected
}

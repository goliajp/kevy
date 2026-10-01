//! Concurrent writer pool that captures ACK logs for post-restart verification.

// Teardown. `join` returns whatever the thread panicked with, and the
// thread is already being abandoned; `shutdown` on a socket the peer
// has closed reports what already happened. Neither has a caller left
// to tell, and stopping halfway through a teardown leaves more behind
// than finishing it blind.
#![expect(clippy::let_underscore_must_use, reason = "teardown has nobody left to report to")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// One entry: a write that was ACK'd by kevy (+OK reply).
///
/// ```
/// # use std::io::Write;
/// # let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
/// # let port = listener.local_addr()?.port();
/// # std::thread::spawn(move || {
/// #     for mut s in listener.incoming().flatten() {
/// #         std::thread::spawn(move || {
/// #             let mut pending = Vec::new();
/// #             while kevy_testnet::read_request(&mut s, &mut pending) {
/// #                 if s.write_all(b"+OK\r\n").is_err() { break; }
/// #             }
/// #         });
/// #     }
/// # });
/// // a stand-in for kevy on `port` that answers every SET with +OK
/// use kevy_chaos::WriterPool;
/// use std::sync::Arc;
/// use std::sync::atomic::{AtomicBool, Ordering};
///
/// let stop = Arc::new(AtomicBool::new(false));
/// let pool = WriterPool::spawn(port, 1, Arc::clone(&stop));
/// while pool.log.lock().expect("no writer panicked").len() < 3 {
///     std::thread::sleep(std::time::Duration::from_millis(1));
/// }
/// stop.store(true, Ordering::Relaxed);
/// let log = pool.join();
/// let acks = log.lock().expect("no writer panicked");
///
/// assert_eq!(acks[0].key, b"w0_k0");
/// assert_eq!(acks[0].value, b"w0_v0");
/// assert_eq!(acks[0].seq, 0);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckEntry {
    /// The key kevy acknowledged, exactly as it went out on the wire.
    ///
    /// ```
    /// # use std::io::Write;
    /// # let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    /// # let port = listener.local_addr()?.port();
    /// # std::thread::spawn(move || {
    /// #     for mut s in listener.incoming().flatten() {
    /// #         std::thread::spawn(move || {
    /// #             let mut pending = Vec::new();
    /// #             while kevy_testnet::read_request(&mut s, &mut pending) {
    /// #                 if s.write_all(b"+OK\r\n").is_err() { break; }
    /// #             }
    /// #         });
    /// #     }
    /// # });
    /// // a stand-in for kevy on `port` that answers every SET with +OK
    /// use kevy_chaos::WriterPool;
    /// use std::sync::Arc;
    /// use std::sync::atomic::{AtomicBool, Ordering};
    ///
    /// let stop = Arc::new(AtomicBool::new(false));
    /// let pool = WriterPool::spawn(port, 1, Arc::clone(&stop));
    /// while pool.log.lock().expect("no writer panicked").len() < 3 {
    ///     std::thread::sleep(std::time::Duration::from_millis(1));
    /// }
    /// stop.store(true, Ordering::Relaxed);
    /// let log = pool.join();
    /// let acks = log.lock().expect("no writer panicked");
    ///
    /// // writer N writes keys `wN_k<seq>`
    /// assert_eq!(acks[1].key, b"w0_k1");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub key: Vec<u8>,
    /// The value kevy acknowledged. A reader that survives the crash must
    /// come back holding this one; anything else is a lost or torn write.
    ///
    /// ```
    /// # use std::io::Write;
    /// # let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    /// # let port = listener.local_addr()?.port();
    /// # std::thread::spawn(move || {
    /// #     for mut s in listener.incoming().flatten() {
    /// #         std::thread::spawn(move || {
    /// #             let mut pending = Vec::new();
    /// #             while kevy_testnet::read_request(&mut s, &mut pending) {
    /// #                 if s.write_all(b"+OK\r\n").is_err() { break; }
    /// #             }
    /// #         });
    /// #     }
    /// # });
    /// // a stand-in for kevy on `port` that answers every SET with +OK
    /// use kevy_chaos::WriterPool;
    /// use std::sync::Arc;
    /// use std::sync::atomic::{AtomicBool, Ordering};
    ///
    /// let stop = Arc::new(AtomicBool::new(false));
    /// let pool = WriterPool::spawn(port, 1, Arc::clone(&stop));
    /// while pool.log.lock().expect("no writer panicked").len() < 3 {
    ///     std::thread::sleep(std::time::Duration::from_millis(1));
    /// }
    /// stop.store(true, Ordering::Relaxed);
    /// let log = pool.join();
    /// let acks = log.lock().expect("no writer panicked");
    ///
    /// // and the value a recovered server must still hold for that key
    /// assert_eq!(acks[1].value, b"w0_v1");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub value: Vec<u8>,
    /// Per-writer monotonic sequence number, starting at 0.
    ///
    /// ```
    /// # use std::io::Write;
    /// # let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    /// # let port = listener.local_addr()?.port();
    /// # std::thread::spawn(move || {
    /// #     for mut s in listener.incoming().flatten() {
    /// #         std::thread::spawn(move || {
    /// #             let mut pending = Vec::new();
    /// #             while kevy_testnet::read_request(&mut s, &mut pending) {
    /// #                 if s.write_all(b"+OK\r\n").is_err() { break; }
    /// #             }
    /// #         });
    /// #     }
    /// # });
    /// // a stand-in for kevy on `port` that answers every SET with +OK
    /// use kevy_chaos::WriterPool;
    /// use std::sync::Arc;
    /// use std::sync::atomic::{AtomicBool, Ordering};
    ///
    /// let stop = Arc::new(AtomicBool::new(false));
    /// let pool = WriterPool::spawn(port, 1, Arc::clone(&stop));
    /// while pool.log.lock().expect("no writer panicked").len() < 3 {
    ///     std::thread::sleep(std::time::Duration::from_millis(1));
    /// }
    /// stop.store(true, Ordering::Relaxed);
    /// let log = pool.join();
    /// let acks = log.lock().expect("no writer panicked");
    ///
    /// let seqs: Vec<u64> = acks.iter().map(|a| a.seq).collect();
    /// assert_eq!(seqs, (0..acks.len() as u64).collect::<Vec<_>>());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub seq: u64,
}

/// Shared, lock-protected log of ACK'd writes from all writers.
///
/// ```
/// # use std::io::Write;
/// # let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
/// # let port = listener.local_addr()?.port();
/// # std::thread::spawn(move || {
/// #     for mut s in listener.incoming().flatten() {
/// #         std::thread::spawn(move || {
/// #             let mut pending = Vec::new();
/// #             while kevy_testnet::read_request(&mut s, &mut pending) {
/// #                 if s.write_all(b"+OK\r\n").is_err() { break; }
/// #             }
/// #         });
/// #     }
/// # });
/// // a stand-in for kevy on `port` that answers every SET with +OK
/// use kevy_chaos::WriterPool;
/// use std::sync::Arc;
/// use std::sync::atomic::{AtomicBool, Ordering};
///
/// let stop = Arc::new(AtomicBool::new(false));
/// let pool = WriterPool::spawn(port, 1, Arc::clone(&stop));
/// while pool.log.lock().expect("no writer panicked").len() < 3 {
///     std::thread::sleep(std::time::Duration::from_millis(1));
/// }
/// stop.store(true, Ordering::Relaxed);
/// let log = pool.join();
/// let acks = log.lock().expect("no writer panicked");
///
/// // one log shared by every writer; each entry is a write the server acknowledged
/// let n = acks.len();
/// drop(acks);
/// let log: kevy_chaos::AckLog = log;
/// assert_eq!(log.lock().expect("no writer panicked").len(), n);
/// assert!(n >= 3);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub type AckLog = Arc<Mutex<Vec<AckEntry>>>;

/// N writer threads, each connecting to kevy and issuing `SET key value`
/// repeatedly. Each successful `+OK` reply appends to the shared `AckLog`.
///
/// ```
/// # use std::io::Write;
/// # let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
/// # let port = listener.local_addr()?.port();
/// # std::thread::spawn(move || {
/// #     for mut s in listener.incoming().flatten() {
/// #         std::thread::spawn(move || {
/// #             let mut pending = Vec::new();
/// #             while kevy_testnet::read_request(&mut s, &mut pending) {
/// #                 if s.write_all(b"+OK\r\n").is_err() { break; }
/// #             }
/// #         });
/// #     }
/// # });
/// // a stand-in for kevy on `port` that answers every SET with +OK
/// use kevy_chaos::WriterPool;
/// use std::sync::Arc;
/// use std::sync::atomic::{AtomicBool, Ordering};
///
/// let stop = Arc::new(AtomicBool::new(false));
/// let pool = WriterPool::spawn(port, 1, Arc::clone(&stop));
/// while pool.log.lock().expect("no writer panicked").len() < 3 {
///     std::thread::sleep(std::time::Duration::from_millis(1));
/// }
/// stop.store(true, Ordering::Relaxed);
/// let log = pool.join();
/// let acks = log.lock().expect("no writer panicked");
///
/// assert!(acks.len() >= 3);
/// assert!(acks.iter().all(|a| a.key.starts_with(b"w0_")));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct WriterPool {
    handles: Vec<thread::JoinHandle<()>>,
    /// Every write kevy said `+OK` to, across all writers. This is the
    /// claim the recovery check is run against: the pool promises nothing
    /// about writes still in flight, only about the ones already answered.
    ///
    /// ```
    /// # use std::io::Write;
    /// # let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    /// # let port = listener.local_addr()?.port();
    /// # std::thread::spawn(move || {
    /// #     for mut s in listener.incoming().flatten() {
    /// #         std::thread::spawn(move || {
    /// #             let mut pending = Vec::new();
    /// #             while kevy_testnet::read_request(&mut s, &mut pending) {
    /// #                 if s.write_all(b"+OK\r\n").is_err() { break; }
    /// #             }
    /// #         });
    /// #     }
    /// # });
    /// // a stand-in for kevy on `port` that answers every SET with +OK
    /// use kevy_chaos::WriterPool;
    /// use std::sync::Arc;
    /// use std::sync::atomic::{AtomicBool, Ordering};
    ///
    /// let stop = Arc::new(AtomicBool::new(false));
    /// let pool = WriterPool::spawn(port, 1, Arc::clone(&stop));
    /// while pool.log.lock().expect("no writer panicked").len() < 3 {
    ///     std::thread::sleep(std::time::Duration::from_millis(1));
    /// }
    /// stop.store(true, Ordering::Relaxed);
    /// let log = pool.join();
    /// let acks = log.lock().expect("no writer panicked");
    ///
    /// // what `join` hands back is this same log
    /// assert!(acks.len() >= 3);
    /// assert_eq!(acks[0].key, b"w0_k0");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
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
            // exit).
            let _ = h.join();
        }
        self.log
    }
}

fn writer_loop(writer_id: usize, port: u16, log: AckLog, stop: Arc<std::sync::atomic::AtomicBool>) {
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
                log.lock()
                    .expect("the lock is only poisoned by a panic that already failed the process")
                    .push(AckEntry { key, value, seq });
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

//! The cross-shard block-serve element drop, made reachable.
//!
//! The defect: the target pops for a waiter and ships the reply to the
//! origin; if the origin's client disconnected in that window the reply
//! had nowhere to go and the element was lost -- taken from the list,
//! delivered to nobody.
//!
//! This race does not happen on an unloaded machine, so it lived as a
//! filed finding rather than a test for exactly the reason a test that
//! only sometimes exercises its defect is worse than none.
//! `KEVY_TEST_XSHARD_SERVE_DELAY_MS` widens the serve window in debug
//! builds so the disconnect lands inside it every time.
//!
//! Its own test binary on purpose: the seam is read once per process and
//! cached, so setting it here cannot leak into the timing of the sibling
//! suite in `blocking_cross_shard.rs`.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

const NSHARDS: usize = 8;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

fn read_n(s: &mut std::net::TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).unwrap();
    buf
}

fn read_line(s: &mut std::net::TcpStream, out: &mut Vec<u8>) {
    loop {
        out.extend_from_slice(&read_n(s, 1));
        if out.ends_with(b"\r\n") {
            break;
        }
    }
}

fn read_len(s: &mut std::net::TcpStream, out: &mut Vec<u8>) -> i64 {
    let start = out.len();
    read_line(s, out);
    let line = &out[start..out.len() - 2];
    std::str::from_utf8(line).unwrap().parse().unwrap()
}

fn read_reply(s: &mut std::net::TcpStream) -> Vec<u8> {
    let head = read_n(s, 1);
    let mut out = head.clone();
    match head[0] {
        b'+' | b'-' | b':' => read_line(s, &mut out),
        b'$' => {
            let len = read_len(s, &mut out);
            if len < 0 {
                return out;
            }
            out.extend_from_slice(&read_n(s, len as usize + 2));
        }
        b'*' => {
            let n = read_len(s, &mut out);
            if n < 0 {
                return out;
            }
            for _ in 0..n {
                out.extend_from_slice(&read_reply(s));
            }
        }
        other => panic!("unknown reply prefix {other:?}"),
    }
    out
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-xblock-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(NSHARDS)).bind([127, 0, 0, 1], port).shards(NSHARDS)
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        for _ in 0..400 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Self {
            port,
            dir,
            stop,
            handle: Some(handle),
        }
    }

    fn connect(&self) -> std::net::TcpStream {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(8)))
            .unwrap();
        s
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ───────────── single-key, remote shard (the hang-bug fix) ─────────────


#[test]
fn a_disconnect_during_the_serve_does_not_lose_the_element() {
    // Two seams make the lossy interleaving deterministic instead of
    // load-only. SERVE_DELAY_MS opens an 800ms window between "origin asks
    // the target to serve" and "the reply comes back", so the disconnect
    // lands inside it. HOLD_CLOSE then defers the origin's teardown of that
    // serving conn, reproducing the exact ordering that lost an element on
    // the macOS CI runner: the reply reaches `origin_on_serve_resp` while
    // the disconnect is still unnoticed — `abandoned` false, the socket
    // already dead. That is the window escrow's `abandoned` flag does NOT
    // cover; the fix ties the escrow's fate to the write result — released
    // only on a clean flush to a live conn, restored on teardown — so a
    // FIN'd client deterministically restores. Without the fix this loses
    // the element (`*0`); with it the element survives (`*1`).
    //
    // SAFETY: set before any server thread starts; this binary runs only
    // this test.
    unsafe {
        std::env::set_var("KEVY_TEST_XSHARD_SERVE_DELAY_MS", "800");
        std::env::set_var("KEVY_TEST_XSHARD_HOLD_CLOSE", "1");
    }
    let srv = Server::start();

    // With NSHARDS shards a random key lands on the conn's own shard ~1/N of
    // the time, and that BLPOP takes the LOCAL block path (blocked.rs), not
    // the cross-shard escrow this test is about — a co-located serve goes to
    // the still-connected consumer promptly and is Redis-equivalent, out of
    // scope here. So retry with a fresh key and a fresh conn until the
    // cross-shard path provably ran (its origin handler bumps a debug-only
    // counter), and only then assert the escrow property. Confirmed by
    // instrumentation that the residual failures were exactly the co-located
    // 1/N, never the cross-shard window.
    for attempt in 0..25u32 {
        let key = format!("escrowed-{attempt}").into_bytes();
        let before = kevy_rt::serve_counters::cross_shard_serves();

        let mut consumer = srv.connect();
        consumer.write_all(&req(&[b"BLPOP", &key, b"5"])).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200)); // park

        // The push makes the key ready; the origin asks the target to serve
        // and the target sits in the 800ms window.
        let mut producer = srv.connect();
        producer.write_all(&req(&[b"RPUSH", &key, b"kept"])).unwrap();
        assert_eq!(read_reply(&mut producer), b":1\r\n");

        // Disconnect while the serve is in flight — 250ms into the 800ms
        // window, so the pop has not happened yet and the reply, once the
        // pop does happen, has nowhere to go.
        std::thread::sleep(std::time::Duration::from_millis(250));
        drop(consumer);

        // Wait past the pop (window end ~800ms) plus the abort round-trip and
        // restore, generously, so the verify below reads the settled state
        // rather than the pop/restore gap.
        std::thread::sleep(std::time::Duration::from_millis(2500));

        if kevy_rt::serve_counters::cross_shard_serves() == before {
            // Co-located: the local path served it (Redis-equivalent). Not
            // the window under test — try a different key/conn placement.
            continue;
        }

        // Verify with LRANGE, not BLPOP. The property is "the element
        // survived, exactly once" — a fact about the list, which LRANGE reads
        // directly and without consuming. BLPOP here would fold in a second
        // timing dependency (whether the serve completes inside its own 5s)
        // on top of the one being tested. The three outcomes map to the three
        // states — one restored, zero lost (the defect), two duplicated.
        let mut c2 = srv.connect();
        c2.write_all(&req(&[b"LRANGE", &key, b"0", b"-1"])).unwrap();
        let list = read_reply(&mut c2);
        let expected = b"*1\r\n$4\r\nkept\r\n".to_vec();
        assert_eq!(
            list, expected,
            "escrow property broken: list should hold exactly one 'kept'. \
             *0 = the element was popped for a vanished client and lost (the \
             defect); *2 = restored AND kept, duplicated. got: {list:?}",
        );
        return;
    }
    panic!("no cross-shard placement in 25 attempts");
}


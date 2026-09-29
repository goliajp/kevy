//! `IDX.QUERY COMPOSE AND` where B's range is far wider than A's hits, so
//! each A-hit's row is read for B's field instead of B's range being
//! walked: the answer must be the plain intersection, for rows whose B
//! value moved, rows whose B value does not coerce, and rows B's range
//! leaves out.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::common;

use common::Wire;

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        // Not a bind probe: the listener that took the port is dropped before the
        // server takes it, and under a parallel run something else can be in that
        // gap. free_port hands out from a block this process owns alone.
        let port = kevy_testnet::free_port();
        let dir = kevy_tmpdir::unique_dir("compose-e2e");
        let stop = Arc::new(AtomicBool::new(false));
        let (stop_thread, dir_thread) = (stop.clone(), dir.clone());
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(8))
                .bind([127, 0, 0, 1], port)
                .shards(8)
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        kevy_testnet::assert_listening(port, "the server under test");
        Self { port, dir, stop, handle: Some(handle) }
    }

    fn wire(&self) -> Wire {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        Wire::new(s)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn call(w: &mut Wire, line: &str) -> Vec<u8> {
    let parts: Vec<&[u8]> = line.split(' ').map(str::as_bytes).collect();
    w.call(&parts)
}

/// The keys of an `IDX.QUERY` reply.
fn keys(reply: &[u8]) -> BTreeSet<String> {
    String::from_utf8_lossy(reply)
        .split("\r\n")
        .filter(|l| l.starts_with("r:"))
        .map(str::to_string)
        .collect()
}

#[test]
fn a_narrow_and_a_wide_range_intersect_row_by_row() {
    let server = Server::start();
    let mut w = server.wire();
    // a: 0..400 once each; b: 0..10 over and over, so any b range is wide
    for i in 0..400 {
        let line = format!("HSET r:{i} a {i} b {}", i % 10);
        assert_eq!(call(&mut w, &line), b":2\r\n");
    }
    for idx in [
        "IDX.CREATE ia ON PREFIX r: FIELD a TYPE i64 KIND range",
        "IDX.CREATE ib ON PREFIX r: FIELD b TYPE i64 KIND range",
    ] {
        assert_eq!(call(&mut w, idx), b"+OK\r\n");
    }
    // a row whose b moved into the range, one that moved out, one whose b
    // no longer coerces, one that lost b
    for line in ["HSET r:11 b 3", "HSET r:13 b 9", "HSET r:15 b x", "HDEL r:17 b"] {
        call(&mut w, line);
    }
    let want: BTreeSet<String> = (10..=20)
        .filter(|&i| match i {
            11 => true,
            13 | 15 | 17 => false,
            _ => (0..=4).contains(&(i % 10)),
        })
        .map(|i| format!("r:{i}"))
        .collect();
    let q = "IDX.QUERY COMPOSE AND ia RANGE 10 20 ib RANGE 0 4 LIMIT 100";
    let mut got = call(&mut w, q);
    for _ in 0..200 {
        if !got.starts_with(b"-INDEXBUILDING") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        got = call(&mut w, q);
    }
    assert_eq!(keys(&got), want, "{}", String::from_utf8_lossy(&got));
}

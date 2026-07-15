//! MULTI…EXEC read-your-writes for multi-key gathers (MGET / MSET).
//!
//! Regression for a confirmed cross-shard ordering bug: inside one
//! transaction a queued single-key write (`SET`) is forwarded on the
//! *batched* request lane (buffered in `request_batch`, flushed once per
//! reactor iteration), while a queued multi-key gather (`MGET`) is
//! dispatched on the *immediate* lane (`send_to` at dispatch time). The
//! gather's request therefore reached the owning shard BEFORE the still-
//! buffered write, so the gather read stale (nil) state — a read-your-
//! writes violation. A single-key `GET` after the same `SET` works because
//! it rides the SAME batched lane (order preserved).
//!
//! Run against an 8-shard reactor so most keys route to a shard other than
//! the connection's owning shard — the exact condition that triggers the
//! two-lane reorder. Single-shard servers never hit it (all inline).

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

/// A `$len\r\nval\r\n` bulk string frame.
fn bulk(v: &[u8]) -> Vec<u8> {
    let mut out = format!("${}\r\n", v.len()).into_bytes();
    out.extend_from_slice(v);
    out.extend_from_slice(b"\r\n");
    out
}

/// A `*N\r\n` + each element's bulk (or `$-1\r\n` for None) — the MGET shape.
fn mget_array(vals: &[Option<&[u8]>]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", vals.len()).into_bytes();
    for v in vals {
        match v {
            Some(b) => out.extend_from_slice(&bulk(b)),
            None => out.extend_from_slice(b"$-1\r\n"),
        }
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
    fn start() -> Server {
        let _gate = START_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-ryow-{}",
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
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(NSHARDS))
                .bind([127, 0, 0, 1], port)
                .shards(NSHARDS)
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        let mut ready = false;
        for _ in 0..400 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(ready, "runtime did not come up");
        Server {
            port,
            dir,
            stop,
            handle: Some(handle),
        }
    }

    fn connect(&self) -> std::net::TcpStream {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        s
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Read exactly `expected.len()` bytes and assert equality.
fn expect(s: &mut std::net::TcpStream, expected: &[u8]) {
    let mut buf = vec![0u8; expected.len()];
    s.read_exact(&mut buf).unwrap_or_else(|e| {
        panic!(
            "read_exact failed ({e}); wanted {:?}",
            String::from_utf8_lossy(expected)
        )
    });
    assert_eq!(
        buf,
        expected,
        "\n expected {:?}\n got      {:?}",
        String::from_utf8_lossy(expected),
        String::from_utf8_lossy(&buf),
    );
}

/// The core bug: `SET k v` … `MGET k` inside one MULTI must read back `v`,
/// not nil — for keys that route to a shard other than the connection's.
#[test]
fn mget_in_multi_sees_earlier_writes_cross_shard() {
    let srv = Server::start();
    let mut c = srv.connect();

    // 32 keys spread across 8 shards: whichever shard owns the conn, the
    // large majority route remote — the reorder condition.
    let n = 32usize;
    let keys: Vec<String> = (0..n).map(|i| format!("ryow:{i}")).collect();
    let vals: Vec<String> = (0..n).map(|i| format!("v{i:03}")).collect();

    c.write_all(&req(&[b"MULTI"])).unwrap();
    expect(&mut c, b"+OK\r\n");
    for i in 0..n {
        c.write_all(&req(&[b"SET", keys[i].as_bytes(), vals[i].as_bytes()]))
            .unwrap();
        expect(&mut c, b"+QUEUED\r\n");
    }
    let mut mget = vec![b"MGET".as_slice()];
    for k in &keys {
        mget.push(k.as_bytes());
    }
    c.write_all(&req(&mget)).unwrap();
    expect(&mut c, b"+QUEUED\r\n");

    c.write_all(&req(&[b"EXEC"])).unwrap();

    // EXEC array = n SET replies (+OK) then the MGET array with every value.
    let mut want = format!("*{}\r\n", n + 1).into_bytes();
    for _ in 0..n {
        want.extend_from_slice(b"+OK\r\n");
    }
    let want_vals: Vec<Option<&[u8]>> = vals.iter().map(|v| Some(v.as_bytes())).collect();
    want.extend_from_slice(&mget_array(&want_vals));
    expect(&mut c, &want);
}

/// `MSET k1 v1 k2 v2 …` then `MGET k1 k2 …` inside one MULTI. Both ride the
/// immediate lane, but the ordering must still hold cross-shard.
#[test]
fn mset_then_mget_in_multi_cross_shard() {
    let srv = Server::start();
    let mut c = srv.connect();

    let n = 16usize;
    let keys: Vec<String> = (0..n).map(|i| format!("mset:{i}")).collect();
    let vals: Vec<String> = (0..n).map(|i| format!("m{i:03}")).collect();

    c.write_all(&req(&[b"MULTI"])).unwrap();
    expect(&mut c, b"+OK\r\n");

    let mut mset = vec![b"MSET".as_slice()];
    for i in 0..n {
        mset.push(keys[i].as_bytes());
        mset.push(vals[i].as_bytes());
    }
    c.write_all(&req(&mset)).unwrap();
    expect(&mut c, b"+QUEUED\r\n");

    let mut mget = vec![b"MGET".as_slice()];
    for k in &keys {
        mget.push(k.as_bytes());
    }
    c.write_all(&req(&mget)).unwrap();
    expect(&mut c, b"+QUEUED\r\n");

    c.write_all(&req(&[b"EXEC"])).unwrap();

    // *2 : +OK (MSET) then the MGET array.
    let mut want = b"*2\r\n+OK\r\n".to_vec();
    let want_vals: Vec<Option<&[u8]>> = vals.iter().map(|v| Some(v.as_bytes())).collect();
    want.extend_from_slice(&mget_array(&want_vals));
    expect(&mut c, &want);
}

/// Interleaved SET / MGET inside one MULTI: each gather must see every write
/// that preceded it in queue order.
#[test]
fn interleaved_set_mget_in_multi_cross_shard() {
    let srv = Server::start();
    let mut c = srv.connect();

    c.write_all(&req(&[b"MULTI"])).unwrap();
    expect(&mut c, b"+OK\r\n");

    // Keys chosen to land on different shards (distinct suffixes over 8
    // shards). Even if one is conn-local, at least one is remote.
    c.write_all(&req(&[b"SET", b"il:a", b"AA"])).unwrap();
    expect(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"MGET", b"il:a", b"il:b"])).unwrap();
    expect(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"SET", b"il:b", b"BB"])).unwrap();
    expect(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"MGET", b"il:a", b"il:b"])).unwrap();
    expect(&mut c, b"+QUEUED\r\n");

    c.write_all(&req(&[b"EXEC"])).unwrap();

    // *4: +OK, [AA, nil], +OK, [AA, BB]
    let mut want = b"*4\r\n+OK\r\n".to_vec();
    want.extend_from_slice(&mget_array(&[Some(b"AA"), None]));
    want.extend_from_slice(b"+OK\r\n");
    want.extend_from_slice(&mget_array(&[Some(b"AA"), Some(b"BB")]));
    expect(&mut c, &want);
}

/// The single-key control: `GET` after `SET` in the same MULTI already
/// worked (same batched lane) — guard against a regression from the fix.
#[test]
fn get_in_multi_still_sees_earlier_write_cross_shard() {
    let srv = Server::start();
    let mut c = srv.connect();

    // Try several keys so at least one is conn-remote regardless of the
    // accept shard; each SET…GET pair must round-trip its value.
    let keys = ["g:0", "g:1", "g:2", "g:3", "g:4", "g:5", "g:6", "g:7"];
    c.write_all(&req(&[b"MULTI"])).unwrap();
    expect(&mut c, b"+OK\r\n");
    for (i, k) in keys.iter().enumerate() {
        let v = format!("gv{i}");
        c.write_all(&req(&[b"SET", k.as_bytes(), v.as_bytes()]))
            .unwrap();
        expect(&mut c, b"+QUEUED\r\n");
        c.write_all(&req(&[b"GET", k.as_bytes()])).unwrap();
        expect(&mut c, b"+QUEUED\r\n");
    }
    c.write_all(&req(&[b"EXEC"])).unwrap();

    let mut want = format!("*{}\r\n", keys.len() * 2).into_bytes();
    for i in 0..keys.len() {
        want.extend_from_slice(b"+OK\r\n");
        want.extend_from_slice(&bulk(format!("gv{i}").as_bytes()));
    }
    expect(&mut c, &want);
}

/// Non-transactional cross-shard MGET must still fan out and return every
/// value (the async fan-out the fix must not break).
#[test]
fn nontxn_mget_fans_out_cross_shard() {
    let srv = Server::start();
    let mut c = srv.connect();

    let n = 24usize;
    let keys: Vec<String> = (0..n).map(|i| format!("fan:{i}")).collect();
    let vals: Vec<String> = (0..n).map(|i| format!("f{i:03}")).collect();

    for i in 0..n {
        c.write_all(&req(&[b"SET", keys[i].as_bytes(), vals[i].as_bytes()]))
            .unwrap();
        expect(&mut c, b"+OK\r\n");
    }
    let mut mget = vec![b"MGET".as_slice()];
    for k in &keys {
        mget.push(k.as_bytes());
    }
    c.write_all(&req(&mget)).unwrap();

    let want_vals: Vec<Option<&[u8]>> = vals.iter().map(|v| Some(v.as_bytes())).collect();
    expect(&mut c, &mget_array(&want_vals));
}

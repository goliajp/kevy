//! v0.4 detection: the thread-per-core runtime must behave as ONE keyspace even
//! though keys are sharded across cores and connections land on arbitrary cores
//! (via SO_REUSEPORT). A value SET through one connection must be visible through
//! another connection that may be served by a different core — proving cross-core
//! routing + per-connection reply ordering + fan-out aggregation.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Serialize server startup across this binary's parallel tests. Each `Server`
/// picks a free ephemeral port then binds it with `SO_REUSEPORT`; two tests
/// racing in the `free_port()`→bind window could pick the *same* port and have
/// the kernel load-balance a connection to the wrong runtime. Holding this gate
/// from `free_port()` until our runtime is bound closes that window.
static START_GATE: Mutex<()> = Mutex::new(());

/// Pick a free localhost port, then free it for the runtime to re-bind.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// Build a RESP multi-bulk request.
fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

fn read_reply(s: &mut std::net::TcpStream, expected: &[u8]) {
    let mut buf = vec![0u8; expected.len()];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(
        &buf,
        expected,
        "expected {:?}",
        String::from_utf8_lossy(expected)
    );
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(nshards: usize) -> Server {
        // Held until our runtime is confirmed bound (see START_GATE).
        let _gate = START_GATE.lock().unwrap_or_else(|e| e.into_inner());
        let port = free_port();
        // Isolate persistence per test run (each write hits an AOF now).
        let dir = std::env::temp_dir().join(format!(
            "kevy-sharded-{}",
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
            let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, nshards, kevy::KevyCommands)
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        // Wait until the port accepts connections.
        let mut ready = false;
        for _ in 0..200 {
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
        std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap()
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

#[test]
fn keyspace_is_shared_across_cores() {
    let srv = Server::start(4);

    // Writer connection sets 200 keys (each hashing to some shard).
    let mut writer = srv.connect();
    for i in 0..200u32 {
        let key = format!("k{i}");
        let val = format!("v{i}");
        writer
            .write_all(&req(&[b"SET", key.as_bytes(), val.as_bytes()]))
            .unwrap();
        read_reply(&mut writer, b"+OK\r\n");
    }

    // A *different* connection (likely a different core) reads them all back.
    let mut reader = srv.connect();
    for i in 0..200u32 {
        let key = format!("k{i}");
        let want = format!("v{i}");
        reader.write_all(&req(&[b"GET", key.as_bytes()])).unwrap();
        let expected = format!("${}\r\n{}\r\n", want.len(), want);
        read_reply(&mut reader, expected.as_bytes());
    }
}

#[test]
fn pipelined_order_is_preserved() {
    // Many commands in one write; replies must come back in request order even
    // though they execute on different cores asynchronously.
    let srv = Server::start(4);
    let mut c = srv.connect();

    let mut batch = Vec::new();
    let mut expected = Vec::new();
    for i in 0..50u32 {
        let key = format!("ord{i}");
        batch.extend_from_slice(&req(&[b"SET", key.as_bytes(), format!("{i}").as_bytes()]));
        expected.extend_from_slice(b"+OK\r\n");
        batch.extend_from_slice(&req(&[b"GET", key.as_bytes()]));
        let v = format!("{i}");
        expected.extend_from_slice(format!("${}\r\n{}\r\n", v.len(), v).as_bytes());
    }
    c.write_all(&batch).unwrap();
    let mut got = vec![0u8; expected.len()];
    c.read_exact(&mut got).unwrap();
    assert_eq!(got, expected, "pipelined replies out of order");
}

#[test]
fn fanout_dbsize_del_flush() {
    let srv = Server::start(4);
    let mut c = srv.connect();

    for i in 0..30u32 {
        c.write_all(&req(&[b"SET", format!("f{i}").as_bytes(), b"x"]))
            .unwrap();
        read_reply(&mut c, b"+OK\r\n");
    }
    // DBSIZE fans out to all shards and sums.
    c.write_all(&req(&[b"DBSIZE"])).unwrap();
    read_reply(&mut c, b":30\r\n");

    // Multi-key DEL spanning shards returns the total removed.
    c.write_all(&req(&[b"DEL", b"f0", b"f1", b"f2", b"nope"]))
        .unwrap();
    read_reply(&mut c, b":3\r\n");

    c.write_all(&req(&[b"DBSIZE"])).unwrap();
    read_reply(&mut c, b":27\r\n");

    // FLUSHALL clears every shard.
    c.write_all(&req(&[b"FLUSHALL"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"DBSIZE"])).unwrap();
    read_reply(&mut c, b":0\r\n");
}

#[test]
fn hash_type_across_cores() {
    let srv = Server::start(4);

    // Write a hash through one connection.
    let mut w = srv.connect();
    w.write_all(&req(&[
        b"HSET", b"user:1", b"name", b"alice", b"age", b"30",
    ]))
    .unwrap();
    read_reply(&mut w, b":2\r\n");
    w.write_all(&req(&[b"HSET", b"user:1", b"age", b"31"]))
        .unwrap(); // update, 0 new
    read_reply(&mut w, b":0\r\n");

    // Read it through another connection (possibly a different core).
    let mut r = srv.connect();
    r.write_all(&req(&[b"HGET", b"user:1", b"name"])).unwrap();
    read_reply(&mut r, b"$5\r\nalice\r\n");
    r.write_all(&req(&[b"HLEN", b"user:1"])).unwrap();
    read_reply(&mut r, b":2\r\n");
    r.write_all(&req(&[b"HINCRBY", b"user:1", b"age", b"1"]))
        .unwrap();
    read_reply(&mut r, b":32\r\n");
    r.write_all(&req(&[b"TYPE", b"user:1"])).unwrap();
    read_reply(&mut r, b"+hash\r\n");

    // WRONGTYPE: a string command on a hash key.
    r.write_all(&req(&[b"GET", b"user:1"])).unwrap();
    let mut buf = [0u8; 64];
    let n = r.read(&mut buf).unwrap();
    assert!(
        buf[..n].starts_with(b"-WRONGTYPE"),
        "got {:?}",
        String::from_utf8_lossy(&buf[..n])
    );
}

#[test]
fn list_type_across_cores() {
    let srv = Server::start(4);
    let mut w = srv.connect();
    w.write_all(&req(&[b"RPUSH", b"q", b"a", b"b", b"c"]))
        .unwrap();
    read_reply(&mut w, b":3\r\n");
    w.write_all(&req(&[b"LPUSH", b"q", b"z"])).unwrap();
    read_reply(&mut w, b":4\r\n");

    let mut r = srv.connect();
    r.write_all(&req(&[b"LRANGE", b"q", b"0", b"-1"])).unwrap();
    read_reply(
        &mut r,
        b"*4\r\n$1\r\nz\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n",
    );
    r.write_all(&req(&[b"LPOP", b"q"])).unwrap();
    read_reply(&mut r, b"$1\r\nz\r\n");
    r.write_all(&req(&[b"LLEN", b"q"])).unwrap();
    read_reply(&mut r, b":3\r\n");
}

/// Read a RESP integer-prefixed line (`<prefix><n>\r\n`) and return `n`.
fn read_len(s: &mut std::net::TcpStream, prefix: u8) -> i64 {
    let mut b = [0u8; 1];
    s.read_exact(&mut b).unwrap();
    assert_eq!(b[0], prefix, "unexpected RESP prefix");
    let mut num = Vec::new();
    loop {
        let mut c = [0u8; 1];
        s.read_exact(&mut c).unwrap();
        if c[0] == b'\r' {
            s.read_exact(&mut [0u8; 1]).unwrap(); // \n
            break;
        }
        num.push(c[0]);
    }
    String::from_utf8(num).unwrap().parse().unwrap()
}

/// Read a RESP array of bulk strings and return its elements sorted (the runtime
/// returns set-algebra results in arbitrary order).
fn read_array_sorted(s: &mut std::net::TcpStream) -> Vec<Vec<u8>> {
    let n = read_len(s, b'*');
    let mut items = Vec::new();
    for _ in 0..n {
        let len = read_len(s, b'$') as usize;
        let mut buf = vec![0u8; len];
        s.read_exact(&mut buf).unwrap();
        s.read_exact(&mut [0u8; 2]).unwrap(); // \r\n
        items.push(buf);
    }
    items.sort();
    items
}

#[test]
fn cross_shard_multikey() {
    let srv = Server::start(4);
    let mut c = srv.connect();

    // MSET routes each pair to its key's shard.
    c.write_all(&req(&[b"MSET", b"a", b"1", b"b", b"2", b"c", b"3"]))
        .unwrap();
    read_reply(&mut c, b"+OK\r\n");
    // MGET preserves request order, nil for a missing key.
    c.write_all(&req(&[b"MGET", b"a", b"missing", b"c"]))
        .unwrap();
    read_reply(&mut c, b"*3\r\n$1\r\n1\r\n$-1\r\n$1\r\n3\r\n");

    // Two sets, likely on different shards.
    c.write_all(&req(&[b"SADD", b"s1", b"x", b"y", b"z"]))
        .unwrap();
    read_reply(&mut c, b":3\r\n");
    c.write_all(&req(&[b"SADD", b"s2", b"y", b"z", b"w"]))
        .unwrap();
    read_reply(&mut c, b":3\r\n");

    c.write_all(&req(&[b"SINTER", b"s1", b"s2"])).unwrap();
    assert_eq!(
        read_array_sorted(&mut c),
        vec![b"y".to_vec(), b"z".to_vec()]
    );
    c.write_all(&req(&[b"SUNION", b"s1", b"s2"])).unwrap();
    assert_eq!(
        read_array_sorted(&mut c),
        vec![b"w".to_vec(), b"x".to_vec(), b"y".to_vec(), b"z".to_vec()]
    );
    c.write_all(&req(&[b"SDIFF", b"s1", b"s2"])).unwrap();
    assert_eq!(read_array_sorted(&mut c), vec![b"x".to_vec()]);
}

#[test]
fn keys_scan_randomkey_across_cores() {
    let srv = Server::start(4);
    let mut c = srv.connect();
    for i in 0..6u32 {
        c.write_all(&req(&[b"SET", format!("u:{i}").as_bytes(), b"x"]))
            .unwrap();
        read_reply(&mut c, b"+OK\r\n");
    }
    c.write_all(&req(&[b"SET", b"other", b"y"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");

    c.write_all(&req(&[b"KEYS", b"u:*"])).unwrap();
    assert_eq!(read_array_sorted(&mut c).len(), 6);
    c.write_all(&req(&[b"KEYS", b"*"])).unwrap();
    assert_eq!(read_array_sorted(&mut c).len(), 7);

    // SCAN replies [cursor "0", [keys]].
    c.write_all(&req(&[b"SCAN", b"0", b"MATCH", b"u:*"]))
        .unwrap();
    assert_eq!(read_len(&mut c, b'*'), 2);
    let curlen = read_len(&mut c, b'$') as usize;
    let mut cur = vec![0u8; curlen];
    c.read_exact(&mut cur).unwrap();
    c.read_exact(&mut [0u8; 2]).unwrap();
    assert_eq!(cur, b"0");
    assert_eq!(read_array_sorted(&mut c).len(), 6);

    // RANDOMKEY returns some existing key.
    c.write_all(&req(&[b"RANDOMKEY"])).unwrap();
    let l = read_len(&mut c, b'$');
    assert!(l > 0);
    c.read_exact(&mut vec![0u8; l as usize]).unwrap();
    c.read_exact(&mut [0u8; 2]).unwrap();
}

#[test]
fn pubsub_across_cores() {
    let srv = Server::start(4);

    // Subscriber registers on its core (read the confirmation to ensure it's live).
    let mut sub = srv.connect();
    sub.write_all(&req(&[b"SUBSCRIBE", b"news"])).unwrap();
    read_reply(&mut sub, b"*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n");

    // Publisher (likely a different core) publishes; receiver count == 1.
    let mut publisher = srv.connect();
    publisher
        .write_all(&req(&[b"PUBLISH", b"news", b"hello"]))
        .unwrap();
    read_reply(&mut publisher, b":1\r\n");

    // The message is delivered to the subscriber across cores.
    read_reply(
        &mut sub,
        b"*3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$5\r\nhello\r\n",
    );

    // Publishing to a channel with no subscribers returns 0.
    publisher
        .write_all(&req(&[b"PUBLISH", b"empty", b"x"]))
        .unwrap();
    read_reply(&mut publisher, b":0\r\n");
}

#[test]
fn transactions() {
    let srv = Server::start(4);
    let mut c = srv.connect();

    c.write_all(&req(&[b"MULTI"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"SET", b"tx:a", b"1"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"INCR", b"tx:a"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"GET", b"tx:a"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"EXEC"])).unwrap();
    // One array: [+OK, :2, $1 "2"].
    read_reply(&mut c, b"*3\r\n+OK\r\n:2\r\n$1\r\n2\r\n");
    c.write_all(&req(&[b"GET", b"tx:a"])).unwrap();
    read_reply(&mut c, b"$1\r\n2\r\n"); // persisted

    // DISCARD drops the queued writes.
    c.write_all(&req(&[b"MULTI"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"SET", b"tx:b", b"x"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"DISCARD"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"GET", b"tx:b"])).unwrap();
    read_reply(&mut c, b"$-1\r\n");

    // EXEC without MULTI errors.
    c.write_all(&req(&[b"EXEC"])).unwrap();
    let mut buf = [0u8; 64];
    let n = c.read(&mut buf).unwrap();
    assert!(buf[..n].starts_with(b"-ERR EXEC without MULTI"));
}

#[test]
fn single_shard_still_works() {
    // N=1 degenerates to a single-core path through the same machinery.
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"SET", b"a", b"1"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"INCR", b"a"])).unwrap();
    read_reply(&mut c, b":2\r\n");
    c.write_all(&req(&[b"GET", b"a"])).unwrap();
    read_reply(&mut c, b"$1\r\n2\r\n");
}

#[test]
fn pipelined_cross_shard_no_deadlock() {
    // Pipeline far more cross-shard commands than one ring holds, in a single
    // write, so the origin shard fans out faster than peers can drain —
    // exercising the SPSC-ring overflow→backlog path and proving the all-to-all
    // mesh never deadlocks, drops, or reorders a reply.
    let srv = Server::start(4);
    let mut c = srv.connect();
    // Fail fast instead of hanging forever if a deadlock regression slips in.
    c.set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .unwrap();
    let n = 10_000usize;
    let mut buf = Vec::new();
    for i in 0..n {
        let key = format!("pp:{i}"); // distinct keys → spread across all 4 shards
        buf.extend_from_slice(&req(&[b"INCR", key.as_bytes()]));
    }
    c.write_all(&buf).unwrap();
    // Each distinct key's first INCR replies `:1`, in request order.
    let mut expected = Vec::with_capacity(n * 4);
    for _ in 0..n {
        expected.extend_from_slice(b":1\r\n");
    }
    let mut got = vec![0u8; expected.len()];
    c.read_exact(&mut got).unwrap();
    assert_eq!(got, expected);
}

//! The read-only RESP listener against a live embedded store.

use std::io::{Read, Write};

use kevy_embedded::{Config, Store};

fn cmd(s: &mut std::net::TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    let mut req = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        req.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        req.extend_from_slice(p);
        req.extend_from_slice(b"\r\n");
    }
    s.write_all(&req).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(40));
    let mut buf = [0u8; 65536];
    let n = s.read(&mut buf).unwrap();
    buf[..n].to_vec()
}

#[test]
fn listener_reads_live_store_rejects_writes() {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let store = Store::open(
        Config::default().with_ttl_reaper_manual().with_resp_listener(addr).with_feed(1 << 20),
    )
    .unwrap();
    store.set(b"greeting", b"hello").unwrap();
    store.hset(b"h:1", &[(b"f", b"v1"), (b"g", b"v2")]).unwrap();
    store.rpush(b"l:1", &[b"a", b"b", b"c"]).unwrap();
    store.sadd(b"s:1", &[b"x", b"y"]).unwrap();
    store.zadd(b"z:1", &[(1.5, b"m1" as &[u8]), (2.5, b"m2")]).unwrap();

    for _ in 0..50 {
        if std::net::TcpStream::connect(addr).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mut c = std::net::TcpStream::connect(addr).unwrap();
    c.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();

    assert_eq!(cmd(&mut c, &[b"PING"]), b"+PONG\r\n");
    assert_eq!(cmd(&mut c, &[b"GET", b"greeting"]), b"$5\r\nhello\r\n");
    assert_eq!(cmd(&mut c, &[b"GET", b"missing"]), b"$-1\r\n");
    assert_eq!(cmd(&mut c, &[b"TYPE", b"h:1"]), b"+hash\r\n");
    assert_eq!(cmd(&mut c, &[b"HGET", b"h:1", b"f"]), b"$2\r\nv1\r\n");
    let r = cmd(&mut c, &[b"HGETALL", b"h:1"]);
    assert!(r.starts_with(b"*4\r\n"), "{:?}", String::from_utf8_lossy(&r));
    assert_eq!(cmd(&mut c, &[b"LRANGE", b"l:1", b"0", b"-1"]), b"*3\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n");
    assert_eq!(cmd(&mut c, &[b"SCARD", b"s:1"]), b":2\r\n");
    let r = cmd(&mut c, &[b"ZRANGE", b"z:1", b"0", b"-1", b"WITHSCORES"]);
    assert!(String::from_utf8_lossy(&r).contains("m2"), "{r:?}");
    assert_eq!(cmd(&mut c, &[b"DBSIZE"]), b":5\r\n");

    // live visibility: a write AFTER connect is immediately readable
    store.set(b"greeting", b"updated").unwrap();
    assert_eq!(cmd(&mut c, &[b"GET", b"greeting"]), b"$7\r\nupdated\r\n");

    // SCAN full walk
    let r = cmd(&mut c, &[b"SCAN", b"0", b"COUNT", b"100"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("greeting") && s.contains("z:1"), "{s}");

    // FEED surface: tail gives the generation; read that generation
    // from offset 0 (reading a stale generation answers Resync — the
    // at-least-once contract).
    let r = cmd(&mut c, &[b"FEED.TAIL"]);
    assert!(r.starts_with(b"*2\r\n"), "{:?}", String::from_utf8_lossy(&r));
    let tail = String::from_utf8_lossy(&r).to_string();
    let generation = tail.lines().nth(1).unwrap().trim_start_matches(':').to_string();
    let r = cmd(&mut c, &[b"FEED.READ", generation.as_bytes(), b"0", b"100"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("greeting"), "feed carries the SET frames: {s}");
    let r = cmd(&mut c, &[b"FEED.READ", b"0", b"0", b"100"]);
    assert!(
        String::from_utf8_lossy(&r).contains("Resync"),
        "stale generation must answer Resync: {:?}",
        String::from_utf8_lossy(&r)
    );

    // writes rejected
    let r = cmd(&mut c, &[b"SET", b"nope", b"x"]);
    assert!(r.starts_with(b"-ERR READONLY"), "{:?}", String::from_utf8_lossy(&r));
    let r = cmd(&mut c, &[b"DEL", b"greeting"]);
    assert!(r.starts_with(b"-ERR READONLY"));
    assert_eq!(store.get(b"greeting").unwrap().unwrap(), b"updated".to_vec());

    // inline command form (redis-cli handshake style)
    c.write_all(b"PING\r\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(40));
    let mut buf = [0u8; 64];
    let n = c.read(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"+PONG\r\n");
}

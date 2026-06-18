//! `PSUBSCRIBE` / `PUNSUBSCRIBE` + `pmessage` cross-core delivery.
//!
//! kevy-client + kevy-embedded already speak the pmessage wire (see
//! `crates/kevy-client/tests/subscribe.rs::psubscribe_then_pmessage_round_trip`);
//! these tests make sure the **server** speaks it too over a real
//! Runtime, including the cross-shard fan-out (a PUBLISH on shard X must
//! reach pattern subscribers on shard Y).

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
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

fn read_reply(s: &mut std::net::TcpStream, expected: &[u8]) {
    let mut buf = vec![0u8; expected.len()];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(
        &buf,
        expected,
        "expected {:?}, got {:?}",
        String::from_utf8_lossy(expected),
        String::from_utf8_lossy(&buf),
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
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-psub-{}",
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
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(10)))
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

#[test]
fn psubscribe_ack_then_pmessage_round_trip() {
    let srv = Server::start(4);

    let mut sub = srv.connect();
    sub.write_all(&req(&[b"PSUBSCRIBE", b"news.*"])).unwrap();
    read_reply(
        &mut sub,
        b"*3\r\n$10\r\npsubscribe\r\n$6\r\nnews.*\r\n:1\r\n",
    );

    // PUBLISH from a different conn (likely a different shard than the
    // sub). reply = 1 (one pattern subscriber).
    let mut pub_ = srv.connect();
    pub_.write_all(&req(&[b"PUBLISH", b"news.tech", b"hi"]))
        .unwrap();
    read_reply(&mut pub_, b":1\r\n");

    read_reply(
        &mut sub,
        b"*4\r\n$8\r\npmessage\r\n$6\r\nnews.*\r\n$9\r\nnews.tech\r\n$2\r\nhi\r\n",
    );
}

#[test]
fn pattern_no_match_no_delivery() {
    let srv = Server::start(4);

    let mut sub = srv.connect();
    sub.write_all(&req(&[b"PSUBSCRIBE", b"news.*"])).unwrap();
    read_reply(
        &mut sub,
        b"*3\r\n$10\r\npsubscribe\r\n$6\r\nnews.*\r\n:1\r\n",
    );

    // Publish to something that does NOT match `news.*` — pub count = 0,
    // sub gets no pmessage frame.
    let mut pub_ = srv.connect();
    pub_.write_all(&req(&[b"PUBLISH", b"weather", b"sunny"]))
        .unwrap();
    read_reply(&mut pub_, b":0\r\n");

    // Verify nothing leaked — set a 200 ms read timeout and confirm EAGAIN.
    sub.set_read_timeout(Some(std::time::Duration::from_millis(200)))
        .unwrap();
    let mut buf = [0u8; 64];
    let r = sub.read(&mut buf);
    match r {
        Err(e) => assert!(matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        )),
        Ok(n) => panic!(
            "expected no data; got {n} bytes: {:?}",
            String::from_utf8_lossy(&buf[..n])
        ),
    }
}

#[test]
fn channel_plus_pattern_both_fire() {
    // SUBSCRIBE chan + PSUBSCRIBE chan — PUBLISH delivers BOTH a `message`
    // and a `pmessage` to the conn (per Redis semantics).
    let srv = Server::start(4);

    let mut sub = srv.connect();
    sub.write_all(&req(&[b"SUBSCRIBE", b"news"])).unwrap();
    read_reply(
        &mut sub,
        b"*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n",
    );
    sub.write_all(&req(&[b"PSUBSCRIBE", b"new?"])).unwrap();
    read_reply(
        &mut sub,
        b"*3\r\n$10\r\npsubscribe\r\n$4\r\nnew?\r\n:2\r\n",
    );

    let mut pub_ = srv.connect();
    pub_.write_all(&req(&[b"PUBLISH", b"news", b"x"])).unwrap();
    // Reply count = 1 channel-precise + 1 pattern-match = 2.
    read_reply(&mut pub_, b":2\r\n");

    // The two frames can arrive in either order. Read 64 bytes and assert
    // both expected substrings are present.
    let msg_frame = b"*3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$1\r\nx\r\n";
    let pmsg_frame = b"*4\r\n$8\r\npmessage\r\n$4\r\nnew?\r\n$4\r\nnews\r\n$1\r\nx\r\n";
    let total = msg_frame.len() + pmsg_frame.len();
    let mut buf = vec![0u8; total];
    sub.read_exact(&mut buf).unwrap();
    let found_msg = buf.windows(msg_frame.len()).any(|w| w == msg_frame);
    let found_pmsg = buf.windows(pmsg_frame.len()).any(|w| w == pmsg_frame);
    assert!(
        found_msg && found_pmsg,
        "expected both frames, got {:?}",
        String::from_utf8_lossy(&buf)
    );
}

#[test]
fn cross_shard_pattern_delivery() {
    // 16-shard server, many subscribers PSUBSCRIBE-ing different patterns
    // — guaranteed to spread across shards. Then PUBLISH a channel that
    // matches each pattern.
    let srv = Server::start(8);

    let mut subs = Vec::new();
    for i in 0..6u32 {
        let mut s = srv.connect();
        let pat = format!("ev.{i}.*");
        s.write_all(&req(&[b"PSUBSCRIBE", pat.as_bytes()])).unwrap();
        let expected = format!(
            "*3\r\n$10\r\npsubscribe\r\n${}\r\n{}\r\n:1\r\n",
            pat.len(),
            pat
        );
        read_reply(&mut s, expected.as_bytes());
        subs.push((s, pat));
    }

    // PUBLISH targeted at ev.3.x — matches only sub #3's pattern.
    let mut pub_ = srv.connect();
    pub_.write_all(&req(&[b"PUBLISH", b"ev.3.start", b"hi"]))
        .unwrap();
    read_reply(&mut pub_, b":1\r\n");

    // Sub #3 receives one pmessage. Other subs receive nothing.
    for (i, (s, pat)) in subs.iter_mut().enumerate() {
        if i == 3 {
            let expected = format!(
                "*4\r\n$8\r\npmessage\r\n${}\r\n{}\r\n$10\r\nev.3.start\r\n$2\r\nhi\r\n",
                pat.len(),
                pat
            );
            read_reply(s, expected.as_bytes());
        } else {
            s.set_read_timeout(Some(std::time::Duration::from_millis(150)))
                .unwrap();
            let mut buf = [0u8; 64];
            let r = s.read(&mut buf);
            assert!(
                r.is_err(),
                "sub #{i} received unexpected data: {:?}",
                r.ok().map(|n| String::from_utf8_lossy(&buf[..n]).into_owned())
            );
        }
    }
}

#[test]
fn punsubscribe_specific_pattern_stops_delivery() {
    let srv = Server::start(4);

    let mut sub = srv.connect();
    sub.write_all(&req(&[b"PSUBSCRIBE", b"a.*", b"b.*"])).unwrap();
    read_reply(
        &mut sub,
        b"*3\r\n$10\r\npsubscribe\r\n$3\r\na.*\r\n:1\r\n",
    );
    read_reply(
        &mut sub,
        b"*3\r\n$10\r\npsubscribe\r\n$3\r\nb.*\r\n:2\r\n",
    );

    sub.write_all(&req(&[b"PUNSUBSCRIBE", b"a.*"])).unwrap();
    read_reply(
        &mut sub,
        b"*3\r\n$12\r\npunsubscribe\r\n$3\r\na.*\r\n:1\r\n",
    );

    // a.X no longer reaches the sub; b.X still does.
    let mut pub_ = srv.connect();
    pub_.write_all(&req(&[b"PUBLISH", b"a.x", b"a"])).unwrap();
    read_reply(&mut pub_, b":0\r\n");
    pub_.write_all(&req(&[b"PUBLISH", b"b.x", b"b"])).unwrap();
    read_reply(&mut pub_, b":1\r\n");
    read_reply(
        &mut sub,
        b"*4\r\n$8\r\npmessage\r\n$3\r\nb.*\r\n$3\r\nb.x\r\n$1\r\nb\r\n",
    );
}

#[test]
fn punsubscribe_all_drains_held_patterns() {
    let srv = Server::start(4);

    let mut sub = srv.connect();
    sub.write_all(&req(&[b"PSUBSCRIBE", b"x.*", b"y.*"])).unwrap();
    read_reply(
        &mut sub,
        b"*3\r\n$10\r\npsubscribe\r\n$3\r\nx.*\r\n:1\r\n",
    );
    read_reply(
        &mut sub,
        b"*3\r\n$10\r\npsubscribe\r\n$3\r\ny.*\r\n:2\r\n",
    );

    // PUNSUBSCRIBE with no args removes everything — one ack per pattern.
    // The two acks can arrive in either order (HashSet iteration order).
    sub.write_all(&req(&[b"PUNSUBSCRIBE"])).unwrap();
    let ack_x = b"*3\r\n$12\r\npunsubscribe\r\n$3\r\nx.*\r\n:1\r\n";
    let ack_y_first = b"*3\r\n$12\r\npunsubscribe\r\n$3\r\ny.*\r\n:1\r\n";
    let ack_x_then_y = b"*3\r\n$12\r\npunsubscribe\r\n$3\r\nx.*\r\n:1\r\n\
                         *3\r\n$12\r\npunsubscribe\r\n$3\r\ny.*\r\n:0\r\n";
    let ack_y_then_x = b"*3\r\n$12\r\npunsubscribe\r\n$3\r\ny.*\r\n:1\r\n\
                         *3\r\n$12\r\npunsubscribe\r\n$3\r\nx.*\r\n:0\r\n";
    let mut buf = vec![0u8; ack_x_then_y.len()];
    sub.read_exact(&mut buf).unwrap();
    assert!(
        buf == ack_x_then_y || buf == ack_y_then_x,
        "unexpected punsubscribe-all wire: {:?}",
        String::from_utf8_lossy(&buf)
    );
    let _ = (ack_x, ack_y_first); // silence unused

    // Publish hits zero subs.
    let mut pub_ = srv.connect();
    pub_.write_all(&req(&[b"PUBLISH", b"x.a", b"v"])).unwrap();
    read_reply(&mut pub_, b":0\r\n");
}

#[test]
fn punsubscribe_with_no_patterns_held_emits_nil_ack() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"PUNSUBSCRIBE"])).unwrap();
    read_reply(
        &mut c,
        b"*3\r\n$12\r\npunsubscribe\r\n$-1\r\n:0\r\n",
    );
}

#[test]
fn subscriber_disconnect_unregisters_patterns() {
    // Drop a sub mid-flight — the pattern registry's count must drop to 0
    // so the next PUBLISH reports 0 receivers (else we'd ghost-deliver).
    let srv = Server::start(4);

    let mut sub = srv.connect();
    sub.write_all(&req(&[b"PSUBSCRIBE", b"gone.*"])).unwrap();
    read_reply(
        &mut sub,
        b"*3\r\n$10\r\npsubscribe\r\n$6\r\ngone.*\r\n:1\r\n",
    );

    // Verify it's wired before we kill it.
    let mut pub_ = srv.connect();
    pub_.write_all(&req(&[b"PUBLISH", b"gone.now", b"v"]))
        .unwrap();
    read_reply(&mut pub_, b":1\r\n");
    read_reply(
        &mut sub,
        b"*4\r\n$8\r\npmessage\r\n$6\r\ngone.*\r\n$8\r\ngone.now\r\n$1\r\nv\r\n",
    );

    drop(sub);
    std::thread::sleep(std::time::Duration::from_millis(80));

    pub_.write_all(&req(&[b"PUBLISH", b"gone.again", b"v"]))
        .unwrap();
    read_reply(&mut pub_, b":0\r\n");
}

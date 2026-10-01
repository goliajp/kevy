//! A global index survives the server's own boot: its catalog, split points
//! included, comes back from the sidecar, and a restart on fewer shards
//! keeps one partition per shard. Driven as a child process because the
//! catalog is loaded by `serve`, which an in-process runtime does not run.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The length of the first whole RESP2 reply in `b`, if it has arrived.
fn reply_len(b: &[u8]) -> Option<usize> {
    let line_end = b.windows(2).position(|w| w == b"\r\n")?;
    let head = std::str::from_utf8(&b[1..line_end]).ok()?;
    match b.first()? {
        b'+' | b'-' | b':' => Some(line_end + 2),
        b'$' => {
            let n: i64 = head.parse().ok()?;
            let len = if n < 0 { line_end + 2 } else { line_end + 2 + n as usize + 2 };
            (b.len() >= len).then_some(len)
        }
        b'*' => {
            let n: i64 = head.parse().ok()?;
            let mut at = line_end + 2;
            for _ in 0..n.max(0) {
                at += reply_len(&b[at..])?;
            }
            Some(at)
        }
        _ => None,
    }
}

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn start(dir: &std::path::Path, shards: usize) -> Server {
        let port = kevy_testnet::free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_kevy"))
            .args(["--port", &port.to_string(), "--threads", &shards.to_string(), "--dir"])
            .arg(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kevy");
        kevy_testnet::assert_listening(port, "the server under test");
        Server { child, port }
    }

    fn call(&self, line: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let words: Vec<&str> = line.split(' ').collect();
        let mut req = format!("*{}\r\n", words.len());
        for w in &words {
            req.push_str(&format!("${}\r\n{w}\r\n", w.len()));
        }
        s.write_all(req.as_bytes()).unwrap();
        let mut buf = Vec::new();
        while reply_len(&buf).is_none() {
            let mut chunk = [0u8; 65536];
            let n = s.read(&mut chunk).unwrap();
            assert!(n > 0, "closed before the reply to {line}");
            buf.extend_from_slice(&chunk[..n]);
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn ready(&self, name: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.call(&format!("IDX.COUNT {name} RANGE 0 0")).starts_with("-INDEXBUILDING") {
            assert!(Instant::now() < deadline, "{name} never became ready");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// `SHUTDOWN`, then wait for the process to go.
    fn stop(mut self) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let _ = s.write_all(b"*1\r\n$8\r\nSHUTDOWN\r\n");
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "the server did not shut down");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The split values of `name`'s `partitioning` pair.
fn splits(srv: &Server, name: &str) -> Vec<String> {
    let d = srv.call(&format!("IDX.DESCRIBE {name}"));
    let Some(at) = d.find("$6\r\nglobal\r\n*") else { return Vec::new() };
    let lines: Vec<&str> = d[at..].split("\r\n").collect();
    let n: usize = lines[2][1..].parse().unwrap();
    (0..n).map(|i| lines[4 + 2 * i].to_string()).collect()
}

#[test]
fn a_global_index_comes_back_after_a_restart_even_on_fewer_shards() {
    let dir = kevy_tmpdir::TmpDir::new("gidx-restart");
    let srv = Server::start(dir.path(), 4);
    for i in 0..2_000 {
        assert_eq!(srv.call(&format!("HSET user:{i} age {}", i % 100)), ":1\r\n");
    }
    let create = "IDX.CREATE g ON PREFIX user: FIELD age TYPE i64 KIND range";
    assert_eq!(
        srv.call(&format!("{create} PARTITION global SPLIT 25 SPLIT 50 SPLIT 75")),
        "+OK\r\n"
    );
    assert_eq!(srv.call(&create.replacen(" g ", " l ", 1)), "+OK\r\n");
    srv.ready("g");
    srv.ready("l");
    let all =
        |srv: &Server, name: &str| srv.call(&format!("IDX.QUERY {name} RANGE 0 100 LIMIT 5000"));
    let rows = all(&srv, "l");
    assert_eq!(all(&srv, "g"), rows);
    srv.stop();

    let srv = Server::start(dir.path(), 4);
    assert_eq!(splits(&srv, "g"), ["25", "50", "75"], "the catalog came back global");
    srv.ready("g");
    assert_eq!(all(&srv, "g"), rows);
    srv.stop();

    // two shards hold at most two partitions: the points thin to one
    let srv = Server::start(dir.path(), 2);
    assert_eq!(splits(&srv, "g"), ["50"]);
    srv.ready("g");
    srv.ready("l");
    assert_eq!(all(&srv, "g"), all(&srv, "l"));
    assert_eq!(all(&srv, "g"), rows);
}

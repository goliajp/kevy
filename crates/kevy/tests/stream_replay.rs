//! Stream writes whose effect depends on when they ran — an `XADD` with a
//! generated ID, a claim gated on idle time — come back from the AOF as
//! they were answered: the same IDs, the same pending owners and counts.

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kevy_testnet::free_port;

fn with_runtime(port: u16, dir: &std::path::Path, nshards: usize, body: impl FnOnce(u16)) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let dir = dir.to_path_buf();
    let handle = std::thread::spawn(move || {
        kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(nshards))
            .bind([127, 0, 0, 1], port)
            .shards(nshards)
            .with_data_dir(dir)
            .run(stop_t)
            .unwrap();
    });
    let up = (0..400).any(|_| {
        std::thread::sleep(Duration::from_millis(5));
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
    });
    assert!(up, "runtime did not start");
    body(port);
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
}

struct Conn(BufReader<std::net::TcpStream>);

impl Conn {
    fn open(port: u16) -> Conn {
        Conn(BufReader::new(std::net::TcpStream::connect(("127.0.0.1", port)).unwrap()))
    }

    fn call(&mut self, cmd: &str) -> String {
        let parts: Vec<&str> = cmd.split(' ').collect();
        let mut req = format!("*{}\r\n", parts.len());
        for p in parts {
            req.push_str(&format!("${}\r\n{p}\r\n", p.len()));
        }
        self.0.get_mut().write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        self.read_one(&mut out);
        out
    }

    /// One whole RESP2 reply, as text.
    fn read_one(&mut self, out: &mut String) {
        let mut line = String::new();
        self.0.read_line(&mut line).unwrap();
        out.push_str(&line);
        let n: i64 = line[1..].trim_end().parse().unwrap_or(0);
        match line.as_bytes()[0] {
            b'$' if n >= 0 => {
                let mut body = vec![0u8; n as usize + 2];
                std::io::Read::read_exact(&mut self.0, &mut body).unwrap();
                out.push_str(&String::from_utf8_lossy(&body));
            }
            b'*' => (0..n.max(0)).for_each(|_| self.read_one(out)),
            _ => {}
        }
    }
}

/// `[id, consumer, idle, count]` rows with the idle column blanked: it is
/// when the state was read, not what it is.
fn blank_idle(reply: &str) -> String {
    let mut t: Vec<&str> = reply.split("\r\n").collect();
    for i in 0..t.len() {
        if t[i] == "*4" && t.get(i + 5).is_some_and(|x| x.starts_with(':')) {
            t[i + 5] = ":idle";
        }
    }
    t.join("\r\n")
}

const READS: &[&str] =
    &["XRANGE s - +", "XRANGE s2 - +", "XPENDING s g - + 10", "XINFO GROUPS s", "XLEN s"];

fn state(c: &mut Conn) -> Vec<String> {
    READS.iter().map(|r| blank_idle(&c.call(r))).collect()
}

#[test]
fn generated_ids_and_idle_claims_replay_as_answered() {
    let dir = kevy_tmpdir::TmpDir::new("stream-replay");
    let mut before = Vec::new();
    with_runtime(free_port(), dir.path(), 1, |p| {
        let mut c = Conn::open(p);
        for i in 0..4 {
            assert!(c.call(&format!("XADD s * f {i}")).starts_with('$'));
            std::thread::sleep(Duration::from_millis(3));
        }
        assert!(c.call("XADD s MAXLEN ~ 3 * f 4").starts_with('$'));
        assert!(c.call("XADD s2 7-* f v").starts_with("$3\r\n7-0"));
        assert!(c.call("XADD s2 7-* f w").starts_with("$3\r\n7-1"));
        assert_eq!(c.call("XGROUP CREATE s g 0"), "+OK\r\n");
        assert!(c.call("XREADGROUP GROUP g c1 STREAMS s >").starts_with("*1"));
        std::thread::sleep(Duration::from_millis(120));
        let first = c.call("XRANGE s - + COUNT 1");
        let id = first.split("\r\n").nth(3).unwrap().to_string();
        assert_eq!(c.call(&format!("XCLAIM s g c2 100 {id} JUSTID")).lines().count(), 3);
        // the second entry goes while pending: the claim below drops it
        // and takes the third, two records from one command
        let second = c.call("XRANGE s - +").split("\r\n").nth(11).unwrap().to_string();
        assert_eq!(c.call(&format!("XDEL s {second}")), ":1\r\n");
        let auto = c.call("XAUTOCLAIM s g c3 100 0 COUNT 3");
        assert!(auto.ends_with(&format!("*1\r\n$15\r\n{second}\r\n")), "{auto}");
        before = state(&mut c);
    });
    with_runtime(free_port(), dir.path(), 1, |p| {
        let after = state(&mut Conn::open(p));
        for ((read, b), a) in READS.iter().zip(&before).zip(&after) {
            assert_eq!(a, b, "{read} changed across the restart");
        }
    });
    // what was compared is the state the writes made, not an empty one
    let pending = &before[2];
    assert!(pending.contains("c2") && pending.contains("c3"), "{pending}");
    assert!(before[0].starts_with("*2\r\n"), "{}", before[0]);
    assert!(pending.starts_with("*2\r\n"), "the dropped entry left the list: {pending}");
}

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

/// The idle column of the extended `XPENDING` rows.
fn idles(reply: &str) -> Vec<i64> {
    let t: Vec<&str> = reply.split("\r\n").collect();
    (0..t.len())
        .filter(|&i| t[i] == "*4" && t.get(i + 5).is_some_and(|x| x.starts_with(':')))
        .map(|i| t[i + 5][1..].parse().unwrap())
        .collect()
}

/// `XINFO CONSUMERS`' idle and inactive columns, which count from the
/// consumer's last contact and last activity, blanked; an inactive of -1
/// (never handed an entry) is kept.
fn blank_consumer_idle(reply: &str) -> String {
    let mut t: Vec<String> = reply.split("\r\n").map(str::to_string).collect();
    for i in 0..t.len() {
        if t[i] == "idle" && i + 1 < t.len() {
            t[i + 1] = ":idle".into();
        }
        if t[i] == "inactive" && i + 1 < t.len() && t[i + 1] != ":-1" {
            t[i + 1] = ":inactive".into();
        }
    }
    t.join("\r\n")
}

const DELIVERY_READS: &[&str] = &["XPENDING s g - + 10", "XINFO GROUPS s", "XINFO CONSUMERS s g"];

fn delivery_state(c: &mut Conn) -> Vec<String> {
    DELIVERY_READS.iter().map(|r| blank_consumer_idle(&blank_idle(&c.call(r)))).collect()
}

/// An entry delivered by `XREADGROUP` keeps its delivery time across a
/// restart: its idle time goes on from where it was instead of starting
/// over. A `NOACK` read moves the group without a pending entry, and a
/// history read changes nothing but the consumer list.
#[test]
fn delivery_times_survive_a_restart() {
    let dir = kevy_tmpdir::TmpDir::new("stream-delivery");
    let (mut before, mut idle_before, mut at) = (Vec::new(), Vec::new(), None);
    with_runtime(free_port(), dir.path(), 1, |p| {
        let mut c = Conn::open(p);
        for id in ["1-1", "2-1", "3-1"] {
            assert!(c.call(&format!("XADD s {id} f v")).starts_with('$'));
        }
        assert_eq!(c.call("XGROUP CREATE s g 0"), "+OK\r\n");
        assert_eq!(c.call("XGROUP CREATE s n 0"), "+OK\r\n");
        assert!(c.call("XREADGROUP GROUP g c1 COUNT 2 STREAMS s >").starts_with("*1"));
        std::thread::sleep(Duration::from_millis(100));
        assert!(c.call("XREADGROUP GROUP g c2 STREAMS s >").starts_with("*1"));
        assert!(c.call("XREADGROUP GROUP n c9 NOACK STREAMS s >").starts_with("*1"));
        assert!(c.call("XREADGROUP GROUP g newbie STREAMS s 0").starts_with("*"));
        assert_eq!(c.call("XREADGROUP GROUP g c1 STREAMS s >"), "*-1\r\n");
        std::thread::sleep(Duration::from_millis(300));
        idle_before = idles(&c.call("XPENDING s g - + 10"));
        at = Some(std::time::Instant::now());
        before = delivery_state(&mut c);
        before.push(c.call("XINFO GROUPS s"));
    });
    let (mut after, mut idle_after, mut elapsed) = (Vec::new(), Vec::new(), 0);
    with_runtime(free_port(), dir.path(), 1, |p| {
        let mut c = Conn::open(p);
        idle_after = idles(&c.call("XPENDING s g - + 10"));
        elapsed = at.expect("the first run read").elapsed().as_millis() as i64;
        after = delivery_state(&mut c);
    });
    for ((read, b), a) in DELIVERY_READS.iter().zip(&before).zip(&after) {
        assert_eq!(a, b, "{read} changed across the restart");
    }
    assert_eq!(idle_before.len(), 3, "three pending entries: {before:?}");
    for (b, a) in idle_before.iter().zip(&idle_after) {
        let gap = a - b;
        assert!(
            (elapsed - 60..=elapsed + 60).contains(&gap),
            "idle went {b} -> {a} over {elapsed} ms: it started over instead of going on"
        );
    }
    // the NOACK group moved to the end with nothing pending, and the
    // history read made a consumer
    let groups = &before[3];
    assert!(groups.contains("$1\r\nn\r\n$9\r\nconsumers\r\n:1\r\n$7\r\npending\r\n:0"), "{groups}");
    assert!(before[2].contains("newbie"), "{}", before[2]);
}

/// `(consumer, idle)` pairs of an `XINFO CONSUMERS` reply, by name.
fn consumer_idles(reply: &str) -> Vec<(String, i64)> {
    let t: Vec<&str> = reply.split("\r\n").collect();
    let mut out: Vec<(String, i64)> = (0..t.len())
        .filter(|&i| t[i] == "name" && t.get(i + 7) == Some(&"idle"))
        .map(|i| (t[i + 2].to_string(), t[i + 8][1..].parse().unwrap()))
        .collect();
    out.sort();
    out
}

/// The consumers' idle times, read now, and when they were read.
fn seen(c: &mut Conn) -> (Vec<(String, i64)>, std::time::Instant) {
    (consumer_idles(&c.call("XINFO CONSUMERS s g")), std::time::Instant::now())
}

/// Each consumer's idle time went on by what passed since `at`, except
/// `polled`'s, whose last contact was an empty read.
fn assert_went_on(
    before: &[(String, i64)],
    after: &[(String, i64)],
    at: std::time::Instant,
    polled: Option<&str>,
) {
    let elapsed = at.elapsed().as_millis() as i64;
    assert_eq!(before.len(), 6, "six consumers: {before:?}");
    for ((name, b), (other, a)) in before.iter().zip(after) {
        assert_eq!(name, other);
        let gap = a - b;
        if polled == Some(name.as_str()) {
            // its last contact was an empty read, which is not recorded:
            // it comes back from its last delivering read, well before
            assert!(gap > elapsed + 60, "{name}: the empty read was recorded ({b} -> {a})");
            continue;
        }
        assert!(
            (elapsed - 80..=elapsed + 80).contains(&gap),
            "{name}: idle went {b} -> {a} over {elapsed} ms: it started over instead of going on"
        );
    }
}

/// A consumer's last contact with its group comes back from the log as
/// it was, however the consumer came about or was last seen: a read that
/// delivered, a history read that made its consumer, `CREATECONSUMER`,
/// and the claims that create their consumer. A read that delivered
/// nothing is not recorded, so a consumer last seen by one comes back with
/// its earlier contact. Twice: from the log as written, and from the log
/// `BGREWRITEAOF` compacts it to.
#[test]
fn consumer_seen_times_survive_a_restart() {
    let dir = kevy_tmpdir::TmpDir::new("stream-seen");
    let mut first = (Vec::new(), std::time::Instant::now());
    with_runtime(free_port(), dir.path(), 1, |p| {
        let mut c = Conn::open(p);
        for id in ["1-1", "2-1", "3-1"] {
            assert!(c.call(&format!("XADD s {id} f v")).starts_with('$'));
        }
        assert_eq!(c.call("XGROUP CREATE s g 0"), "+OK\r\n");
        assert!(c.call("XREADGROUP GROUP g reader COUNT 2 STREAMS s >").starts_with("*1"));
        assert_eq!(c.call("XGROUP CREATECONSUMER s g made"), ":1\r\n");
        std::thread::sleep(Duration::from_millis(60));
        assert!(c.call("XREADGROUP GROUP g idler STREAMS s 0").starts_with('*'));
        assert!(c.call("XCLAIM s g claimer 0 1-1 JUSTID").starts_with("*1"));
        assert!(c.call("XAUTOCLAIM s g auto 0 2-1 COUNT 1 JUSTID").starts_with("*3"));
        std::thread::sleep(Duration::from_millis(60));
        // an existing consumer is seen again by an empty read
        assert!(c.call("XREADGROUP GROUP g late COUNT 1 STREAMS s >").starts_with("*1"));
        assert_eq!(c.call("XREADGROUP GROUP g reader STREAMS s >"), "*-1\r\n");
        std::thread::sleep(Duration::from_millis(300));
        first = seen(&mut c);
    });
    let mut second = (Vec::new(), std::time::Instant::now());
    with_runtime(free_port(), dir.path(), 1, |p| {
        let mut c = Conn::open(p);
        let (now, _) = seen(&mut c);
        assert_went_on(&first.0, &now, first.1, Some("reader"));
        second = seen(&mut c);
        assert_eq!(c.call("BGREWRITEAOF"), "+OK\r\n");
        let aof = dir.path().join("aof-0.aof");
        let compacted = (0..1000).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            std::fs::read(&aof).is_ok_and(|b| b.windows(8).any(|w| w == b"MKSTREAM"))
        });
        assert!(compacted, "the rewritten AOF never swapped in");
    });
    with_runtime(free_port(), dir.path(), 1, |p| {
        let (now, _) = seen(&mut Conn::open(p));
        assert_went_on(&second.0, &now, second.1, None);
    });
}

/// The internal record verb is refused from a client — over the wire and
/// from a script — and changes nothing; the same frame read back from the
/// AOF is applied (the restart tests above).
#[test]
fn the_internal_record_verb_is_refused_from_a_client() {
    let dir = kevy_tmpdir::TmpDir::new("stream-internal");
    with_runtime(free_port(), dir.path(), 1, |p| {
        let mut c = Conn::open(p);
        assert_eq!(c.call("XGROUP CREATE s g $ MKSTREAM"), "+OK\r\n");
        let want = format!("-{}\r\n", kevy_verbs::aof::INTERNAL_REFUSAL);
        assert_eq!(c.call("XINTERNAL.CONSUMERSEEN s g c 1"), want);
        assert_eq!(c.call("xinternal.consumerseen s g c 1"), want);
        // the call splits on spaces, so the script has none
        let script = "return(redis.call('XINTERNAL.CONSUMERSEEN','s','g','c','1'))";
        let reply = c.call(&format!("EVAL {script} 0"));
        assert!(reply.starts_with('-') && reply.contains("not accepted from a client"), "{reply}");
        assert_eq!(c.call("XINFO CONSUMERS s g"), "*0\r\n", "a refused record made a consumer");
    });
}

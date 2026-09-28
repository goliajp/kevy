//! A global index against a real 4-shard reactor: rows stay on the shard
//! their key hashes to, their entries go to the partition their value falls
//! in. It must answer what a local index over the same rows answers, and a
//! write must be visible to a query sent the moment its reply arrives.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::common::Wire;

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(shards: usize) -> Self {
        let port = kevy_testnet::free_port();
        let dir = std::env::temp_dir().join(format!("kevy-gidx-{}-{port}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (stop_thread, dir_thread) = (stop.clone(), dir.clone());
        let handle = std::thread::spawn(move || {
            kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(shards))
                .bind([127, 0, 0, 1], port)
                .shards(shards)
                .with_data_dir(dir_thread)
                .run(stop_thread)
                .unwrap();
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

fn call(w: &mut Wire, q: &[&[u8]]) -> Vec<u8> {
    w.call(q)
}

fn ready(w: &mut Wire, q: &[&[u8]]) -> Vec<u8> {
    for _ in 0..200 {
        let r = call(w, q);
        if !r.starts_with(b"-INDEXBUILDING") {
            return r;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("the index never became ready");
}

fn text(r: &[u8]) -> String {
    String::from_utf8_lossy(r).into_owned()
}

fn create(w: &mut Wire, name: &[u8], tail: &[&[u8]]) {
    let mut q: Vec<&[u8]> = vec![
        b"IDX.CREATE",
        name,
        b"ON",
        b"PREFIX",
        b"user:",
        b"FIELD",
        b"age",
        b"TYPE",
        b"i64",
        b"KIND",
        b"range",
    ];
    q.extend_from_slice(tail);
    assert_eq!(call(w, &q), b"+OK\r\n", "create {}", text(name));
}

#[test]
fn a_global_index_answers_what_a_local_one_does_and_sees_a_write_at_once() {
    let srv = Server::start(4);
    let mut w = srv.wire();
    create(&mut w, b"age_l", &[]);
    create(
        &mut w,
        b"age_g",
        &[b"PARTITION", b"global", b"SPLIT", b"25", b"SPLIT", b"50", b"SPLIT", b"75"],
    );
    ready(&mut w, &[b"IDX.QUERY", b"age_g", b"RANGE", b"0", b"0", b"LIMIT", b"1"]);
    for i in 0..120u32 {
        let (key, age) = (format!("user:{i}"), ((i * 7) % 100).to_string());
        call(&mut w, &[b"HSET", key.as_bytes(), b"age", age.as_bytes()]);
        // the reply waited for the entry's partition owner: no sleep here
        let hit = call(
            &mut w,
            &[b"IDX.QUERY", b"age_g", b"RANGE", age.as_bytes(), age.as_bytes(), b"LIMIT", b"200"],
        );
        assert!(text(&hit).contains(&key), "{key} age {age} not visible at once: {}", text(&hit));
    }
    let all = |w: &mut Wire, name: &[u8]| {
        call(w, &[b"IDX.QUERY", name, b"RANGE", b"0", b"100", b"LIMIT", b"1000"])
    };
    assert_eq!(text(&all(&mut w, b"age_g")), text(&all(&mut w, b"age_l")));

    // a write that moves the entry to another partition
    call(&mut w, &[b"HSET", b"user:3", b"age", b"99"]);
    let moved = call(&mut w, &[b"IDX.QUERY", b"age_g", b"RANGE", b"99", b"99", b"LIMIT", b"200"]);
    assert!(text(&moved).contains("user:3"));
    let old = call(&mut w, &[b"IDX.QUERY", b"age_g", b"RANGE", b"21", b"21", b"LIMIT", b"200"]);
    assert!(!text(&old).contains("user:3"), "the old partition still holds it: {}", text(&old));
    // and one that removes it
    call(&mut w, &[b"DEL", b"user:3"]);
    let gone = call(&mut w, &[b"IDX.QUERY", b"age_g", b"RANGE", b"99", b"99", b"LIMIT", b"200"]);
    assert!(!text(&gone).contains("user:3"));
    assert_eq!(text(&all(&mut w, b"age_g")), text(&all(&mut w, b"age_l")));
}

#[test]
fn a_global_index_describes_its_splits_and_refuses_more_than_the_shards_hold() {
    let srv = Server::start(4);
    let mut w = srv.wire();
    create(&mut w, b"g", &[b"PARTITION", b"global", b"SPLIT", b"10"]);
    let d = text(&call(&mut w, &[b"IDX.DESCRIBE", b"g"]));
    assert!(d.contains("PARTITION") && d.contains("global") && d.contains("SPLIT"), "{d}");
    let too_many: Vec<&[u8]> = vec![
        b"PARTITION",
        b"global",
        b"SPLIT",
        b"1",
        b"SPLIT",
        b"2",
        b"SPLIT",
        b"3",
        b"SPLIT",
        b"4",
    ];
    let mut q: Vec<&[u8]> = vec![
        b"IDX.CREATE",
        b"g5",
        b"ON",
        b"PREFIX",
        b"user:",
        b"FIELD",
        b"age",
        b"TYPE",
        b"i64",
        b"KIND",
        b"range",
    ];
    q.extend_from_slice(&too_many);
    assert!(
        text(&call(&mut w, &q))
            .contains("SPLIT allows at most one point fewer than the shard count")
    );
}

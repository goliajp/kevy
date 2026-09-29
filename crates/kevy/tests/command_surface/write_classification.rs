//! A verb the command registry calls a write is a write to the server:
//! a read-only replica refuses it, and a transaction watching its key
//! sees it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kevy_resp::ops_table::{OP_TABLE, surface};
use kevy_tmpdir::TmpDir;

use crate::common::Wire;

type Cmd<'a> = &'a [&'a [u8]];

const READONLY: &[u8] = b"-READONLY You can't write against a read only replica.\r\n";

struct Node {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    _dir: TmpDir,
}

impl Node {
    fn start() -> Self {
        let port = kevy_testnet::free_port();
        let dir = TmpDir::new("write-classification");
        let dir_thread = dir.path().to_path_buf();
        let commands = kevy::KevyCommands::sharded(1);
        let inboxes = commands.state().take_replica_inboxes().expect("fresh state");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::builder(commands)
                .bind([127, 0, 0, 1], port)
                .shards(1)
                .with_data_dir(dir_thread)
                .with_aof(false)
                .with_replica_inboxes(inboxes);
            rt.run(stop_thread).unwrap();
        });
        kevy_testnet::assert_listening(port, "the server under test");
        Self { port, stop, handle: Some(handle), _dir: dir }
    }

    fn wire(&self) -> Wire {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        Wire::new(s)
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Every write the shared command layer's registry names, and every write
/// row of the server's command table: the gate runs before the arguments
/// are read, so a bare `VERB k` must be refused with Redis's exact reply.
#[test]
fn a_read_only_replica_refuses_every_registry_write() {
    let node = Node::start();
    let mut c = node.wire();
    // an upstream nobody listens on: the node turns replica and stays one
    let dead = kevy_testnet::free_port().to_string();
    assert_eq!(c.call(&[b"REPLICAOF" as &[u8], b"127.0.0.1", dead.as_bytes()]), b"+OK\r\n");

    let mut writes: Vec<&str> =
        kevy_verbs::VERBS.iter().filter(|v| v.write).map(|v| v.name).collect();
    writes.extend(
        OP_TABLE.iter().filter(|o| o.write && o.surfaces & surface::SERVER != 0).map(|o| o.name),
    );
    writes.sort_unstable();
    writes.dedup();
    assert!(writes.len() > 75, "the registries lost rows: {}", writes.len());
    let accepted: Vec<String> = writes
        .iter()
        .filter_map(|name| {
            let reply = c.call(&[name.as_bytes(), b"k"]);
            (reply != READONLY).then(|| format!("{name}: {}", String::from_utf8_lossy(&reply)))
        })
        .collect();
    assert!(accepted.is_empty(), "a read-only replica let writes through: {accepted:#?}");
    // a read on the same node is still served
    assert_eq!(c.call(&[b"GET" as &[u8], b"k"]), b"$-1\r\n");
}

/// A blocking pop that finds data pops at once, and that pop invalidates
/// a `WATCH` on the key exactly as the non-blocking pop does.
#[test]
fn a_blocking_pop_that_pops_invalidates_a_watch() {
    let node = Node::start();
    let cases: &[(Cmd, Cmd)] = &[
        (&[b"RPUSH", b"q", b"a", b"b"], &[b"BLPOP", b"q", b"0"]),
        (&[b"RPUSH", b"q", b"a", b"b"], &[b"BRPOP", b"q", b"0"]),
        (&[b"RPUSH", b"q", b"a", b"b"], &[b"LPOP", b"q"]),
        (&[b"ZADD", b"q", b"1", b"a", b"2", b"b"], &[b"BZPOPMIN", b"q", b"0"]),
        (&[b"RPUSH", b"q", b"a", b"b"], &[b"BRPOPLPUSH", b"q", b"q", b"0"]),
    ];
    let mut kept = Vec::new();
    for (setup, pop) in cases {
        let (mut a, mut b) = (node.wire(), node.wire());
        a.call(&[b"DEL" as &[u8], b"q"]);
        a.call(setup);
        assert_eq!(a.call(&[b"WATCH" as &[u8], b"q"]), b"+OK\r\n");
        let popped = b.call(pop);
        assert!(!popped.starts_with(b"-") && popped != b"*-1\r\n", "{pop:?} popped nothing");
        a.call(&[b"MULTI" as &[u8]]);
        a.call(&[b"SET" as &[u8], b"other", b"1"]);
        if a.call(&[b"EXEC" as &[u8]]) != b"*-1\r\n" {
            kept.push(String::from_utf8_lossy(pop[0]).into_owned());
        }
    }
    assert!(kept.is_empty(), "these pops left the WATCH standing: {kept:?}");
}

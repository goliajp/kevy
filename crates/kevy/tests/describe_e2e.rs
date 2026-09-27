//! `TABLE.DESCRIBE` / `IDX.DESCRIBE` / `VIEW.DESCRIBE` against a real
//! 8-shard server: the `declaration` each reply carries is replayed
//! through the server's own parsers, and the object it recreates must
//! describe byte-for-byte the same. That round trip is what makes
//! `dump --schema` trustworthy; the renderer's unit tests cannot hold it,
//! because the IDX.CREATE and VIEW.CREATE grammars live in this crate.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

mod common;

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
        let dir = std::env::temp_dir().join(format!(
            "kevy-describe-e2e-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
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

fn words(line: &str) -> Vec<Vec<u8>> {
    line.split(' ').map(|w| w.as_bytes().to_vec()).collect()
}

/// The bulk strings of a flat RESP array, in order (nested arrays are
/// walked through: their headers are skipped, their bulks kept).
fn bulks(reply: &[u8]) -> Vec<Vec<u8>> {
    let (mut out, mut i) = (Vec::new(), 0);
    while i < reply.len() {
        let nl = i + reply[i..].windows(2).position(|w| w == b"\r\n").unwrap();
        if reply[i] == b'$' {
            let n: usize = std::str::from_utf8(&reply[i + 1..nl]).unwrap().parse().unwrap();
            out.push(reply[nl + 2..nl + 2 + n].to_vec());
            i = nl + 2 + n + 2;
        } else {
            i = nl + 2;
        }
    }
    out
}

/// `declaration` is every reply's last field: the bulks after its label.
fn declaration(reply: &[u8]) -> Vec<Vec<u8>> {
    let all = bulks(reply);
    let at = all.iter().rposition(|b| b == b"declaration").expect("a declaration field");
    all[at + 1..].to_vec()
}

const DECLARED: &[&str] = &[
    "TABLE.DECLARE order PREFIX order: PK id COLUMN id str COLUMN total f64 COLUMN at i64 COLUMN who str INDEX at range VALUES total who INDEX id unique ORDERPATH recent ON who THEN at DESC WINDOW at SPAN 86400 BUCKET 3600",
    "IDX.CREATE doc.body ON PREFIX doc: FIELDS title body WEIGHTS 2.5 1 TYPE str KIND text WITH POSITIONS VALUES year TYPES i64 MAXMEM 1048576",
    "IDX.CREATE emb ON PREFIX v: FIELD vec TYPE vector KIND ann DIM 4 DISTANCE ip M 8 EF 64",
    "IDX.CREATE spend ON PREFIX order: FIELD total TYPE f64 KIND agg GROUPBY who",
    "IDX.CREATE age ON PREFIX user: FIELD age TYPE i64 KIND range",
    "IDX.CREATE city ON PREFIX user: FIELD city TYPE str KIND unique",
    "VIEW.CREATE adults QUERY ( DIFF age RANGE 18 65 city EQ tokyo ) ORDER BY age DESC MODE materialized TOPK 10",
];

const DESCRIBES: &[&str] = &[
    "TABLE.DESCRIBE order",
    "IDX.DESCRIBE doc.body",
    "IDX.DESCRIBE emb",
    "IDX.DESCRIBE spend",
    "IDX.DESCRIBE age",
    "IDX.DESCRIBE city",
    "VIEW.DESCRIBE adults",
];

const DROPS: &[&str] = &[
    "VIEW.DROP adults",
    "TABLE.DROP order",
    "IDX.DROP doc.body",
    "IDX.DROP emb",
    "IDX.DROP spend",
    "IDX.DROP age",
    "IDX.DROP city",
];

#[test]
fn every_declaration_recreates_an_object_that_describes_the_same() {
    let server = Server::start();
    let mut w = server.wire();
    for line in DECLARED {
        assert_eq!(w.call(&words(line)), b"+OK\r\n", "{line}");
    }
    let before: Vec<Vec<u8>> = DESCRIBES.iter().map(|d| w.call(&words(d))).collect();
    for (line, reply) in DECLARED.iter().zip(&before) {
        assert_eq!(declaration(reply), words(line), "the canonical spelling of {line}");
    }
    for line in DROPS {
        assert_eq!(w.call(&words(line)), b":1\r\n", "{line}");
    }
    for reply in &before {
        assert_eq!(w.call(&declaration(reply)), b"+OK\r\n");
    }
    for (d, reply) in DESCRIBES.iter().zip(&before) {
        assert_eq!(w.call(&words(d)), *reply, "{d} after the replay");
    }
}

#[test]
fn a_compiled_index_names_its_table_and_a_missing_object_names_its_lister() {
    let server = Server::start();
    let mut w = server.wire();
    assert_eq!(w.call(&words(DECLARED[0])), b"+OK\r\n");
    for path in ["order.at", "order.id", "order.recent"] {
        let all = bulks(&w.call(&words(&format!("IDX.DESCRIBE {path}"))));
        let table = all.iter().position(|b| b == b"table").unwrap();
        assert_eq!(all[table + 1], b"order", "{path}");
        assert_eq!(all.last().unwrap(), b"-", "{path} has no declaration of its own");
    }
    for (verb, noun, lister) in [
        ("TABLE", "table", "TABLE.LIST"),
        ("IDX", "index", "IDX.LIST"),
        ("VIEW", "view", "VIEW.LIST"),
    ] {
        assert_eq!(
            w.call(&words(&format!("{verb}.DESCRIBE nope"))),
            format!("-ERR no such {noun} 'nope' ({lister} enumerates them)\r\n").into_bytes()
        );
        assert_eq!(
            w.call(&words(&format!("{verb}.DESCRIBE"))),
            format!("-ERR usage: {verb}.DESCRIBE name\r\n").into_bytes(),
            "{verb}.DESCRIBE arity"
        );
    }
}

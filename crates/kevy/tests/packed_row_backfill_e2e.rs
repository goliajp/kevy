//! A declaration must reach the rows that were already there.
//!
//! The packed representation is applied by the write hook, which only ever
//! sees rows written after their table existed. Three ordinary sequences
//! leave rows behind it — declaring a table over an existing keyspace,
//! restoring a snapshot, and a row nothing writes again — and the first of
//! those is the shape `bench/pgcompare.py` uses. Measured on the box with
//! two million rows, the flag changed RSS by nine bytes per CSV-MB, because
//! not one row had been converted.

use std::io::{Read, Write};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

fn cmd(s: &mut std::net::TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    s.write_all(&req(parts)).unwrap();
    let mut buf = [0u8; 65536];
    let n = s.read(&mut buf).unwrap();
    buf[..n].to_vec()
}

/// `:1234\r\n` → 1234.
fn int(reply: &[u8]) -> i64 {
    let body = std::str::from_utf8(&reply[1..reply.len() - 2]).unwrap();
    body.parse().unwrap_or_else(|_| panic!("not an integer reply: {reply:?}"))
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let dir = std::env::temp_dir().join(format!("kevy-packbf-{port}"));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (stop_t, dir_t) = (stop.clone(), dir.clone());
        let handle = std::thread::spawn(move || {
            kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(2))
                .bind([127, 0, 0, 1], port)
                .shards(2)
                .with_data_dir(dir_t)
                .run(stop_t)
                .unwrap();
        });
        kevy_testnet::assert_listening(port, "the server under test");
        Self { port, dir, stop, handle: Some(handle) }
    }

    fn connect(&self) -> std::net::TcpStream {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        s
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

const ROWS: usize = 400;

fn load_rows(c: &mut std::net::TcpStream) {
    for i in 0..ROWS {
        let (k, id, name, sku) =
            (format!("row:{i}"), i.to_string(), format!("user{i}"), (i % 97).to_string());
        cmd(
            c,
            &[
                b"HSET",
                k.as_bytes(),
                b"id",
                id.as_bytes(),
                b"name",
                name.as_bytes(),
                b"dept",
                b"engineering",
                b"age",
                b"33",
                b"ts",
                b"1700000000",
                b"sku",
                sku.as_bytes(),
            ],
        );
    }
}

fn declare(c: &mut std::net::TcpStream) -> Vec<u8> {
    cmd(
        c,
        &[
            b"TABLE.DECLARE", b"t", b"PREFIX", b"row:", b"PK", b"id",
            b"COLUMN", b"id", b"i64", b"COLUMN", b"name", b"str",
            b"COLUMN", b"dept", b"str", b"COLUMN", b"age", b"i64",
            b"COLUMN", b"ts", b"i64", b"COLUMN", b"sku", b"i64",
        ],
    )
}

/// Wait for `key` to cost less than it did, or give up. Returns the cost.
fn wait_shrunk(c: &mut std::net::TcpStream, key: &[u8], was: i64) -> i64 {
    for _ in 0..80 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let now = int(&cmd(c, &[b"MEMORY", b"USAGE", key]));
        if now < was {
            return now;
        }
    }
    int(&cmd(c, &[b"MEMORY", b"USAGE", key]))
}

#[test]
fn a_declaration_reaches_the_rows_that_preceded_it() {
    let srv = Server::start();
    let mut c = srv.connect();
    assert_eq!(cmd(&mut c, &[b"CONFIG", b"SET", b"packed-rows", b"yes"]), b"+OK\r\n");

    load_rows(&mut c);
    let before = int(&cmd(&mut c, &[b"MEMORY", b"USAGE", b"row:5"]));
    assert_eq!(declare(&mut c), b"+OK\r\n");

    let after = wait_shrunk(&mut c, b"row:5", before);
    assert!(
        after < before,
        "a row written before the declaration still costs {after} (was {before}) — \
         nothing packed it, which is what the box measured as a nine-byte win"
    );

    // The last row, to show the backfill runs to the end of its key list
    // rather than converting the first batch and stopping.
    let last = format!("row:{}", ROWS - 1);
    assert!(int(&cmd(&mut c, &[b"MEMORY", b"USAGE", last.as_bytes()])) < before);

    // And the rows still answer. A backfill that packed them into
    // something unreadable would satisfy every assertion above.
    assert_eq!(
        cmd(&mut c, &[b"HGET", last.as_bytes(), b"name"]),
        format!("${}\r\nuser{}\r\n", 4 + (ROWS - 1).to_string().len(), ROWS - 1).into_bytes()
    );
    assert_eq!(cmd(&mut c, &[b"HLEN", last.as_bytes()]), b":6\r\n");
}

#[test]
fn the_switch_stays_off_until_it_is_asked_for() {
    let srv = Server::start();
    let mut c = srv.connect();
    assert_eq!(read_packed_rows(&mut c), "no", "off by default");

    load_rows(&mut c);
    let before = int(&cmd(&mut c, &[b"MEMORY", b"USAGE", b"row:5"]));
    assert_eq!(declare(&mut c), b"+OK\r\n");
    std::thread::sleep(std::time::Duration::from_millis(600));
    assert_eq!(
        int(&cmd(&mut c, &[b"MEMORY", b"USAGE", b"row:5"])),
        before,
        "a declaration alone must not change the representation"
    );

    assert_eq!(cmd(&mut c, &[b"CONFIG", b"SET", b"packed-rows", b"yes"]), b"+OK\r\n");
    assert_eq!(read_packed_rows(&mut c), "yes");
    assert!(wait_shrunk(&mut c, b"row:5", before) < before, "and turning it on converts them");
}

fn read_packed_rows(c: &mut std::net::TcpStream) -> String {
    let reply = cmd(c, &[b"CONFIG", b"GET", b"packed-rows"]);
    let text = String::from_utf8_lossy(&reply).to_string();
    for want in ["yes", "no"] {
        if text.contains(&format!("\r\n{want}\r\n")) {
            return want.to_string();
        }
    }
    panic!("CONFIG GET packed-rows answered {text:?}");
}

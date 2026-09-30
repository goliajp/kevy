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
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

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
    /// Whether dropping this server also removes the data directory. The
    /// restart half must not: the first server's `Drop` deleting the
    /// snapshot is indistinguishable from the snapshot not restoring.
    owns_dir: bool,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let port = kevy_testnet::free_port();
        Self::spawn(port, std::env::temp_dir().join(format!("kevy-packbf-{port}")), true)
    }

    /// A server over an existing data directory — the restart half.
    fn start_in(dir: &std::path::Path) -> Self {
        let port = kevy_testnet::free_port();
        Self::spawn(port, dir.to_path_buf(), false)
    }

    fn spawn(port: u16, dir: std::path::PathBuf, owns_dir: bool) -> Self {
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
        Self { port, dir, owns_dir, stop, handle: Some(handle) }
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
        if self.owns_dir {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
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
            b"TABLE.DECLARE",
            b"t",
            b"PREFIX",
            b"row:",
            b"PK",
            b"id",
            b"COLUMN",
            b"id",
            b"i64",
            b"COLUMN",
            b"name",
            b"str",
            b"COLUMN",
            b"dept",
            b"str",
            b"COLUMN",
            b"age",
            b"i64",
            b"COLUMN",
            b"ts",
            b"i64",
            b"COLUMN",
            b"sku",
            b"i64",
        ],
    )
}

/// Wait for `key` to cost less than it did, or give up. Returns the cost.
fn wait_shrunk(c: &mut std::net::TcpStream, key: &[u8], was: i64) -> i64 {
    // the backfill runs on the tick, in batches, behind whatever else the
    // process is doing
    let deadline =
        std::time::Instant::now() + kevy_testnet::patience(std::time::Duration::from_secs(4));
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let now = int(&cmd(c, &[b"MEMORY", b"USAGE", key]));
        if now < was || std::time::Instant::now() >= deadline {
            return now;
        }
    }
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
    assert!(wait_shrunk(&mut c, last.as_bytes(), before) < before);

    // And the rows still answer. A backfill that packed them into
    // something unreadable would satisfy every assertion above.
    assert_eq!(
        cmd(&mut c, &[b"HGET", last.as_bytes(), b"name"]),
        format!("${}\r\nuser{}\r\n", 4 + (ROWS - 1).to_string().len(), ROWS - 1).into_bytes()
    );
    assert_eq!(cmd(&mut c, &[b"HLEN", last.as_bytes()]), b":6\r\n");
}

#[test]
fn the_switch_can_be_turned_off_and_the_rows_stay_readable() {
    // Named for what it now proves. Until 5.4.1 the default was off and this
    // asserted that nothing packed until asked; the default is on, so the
    // property worth holding is the other one — a deployment that wants
    // 5.4.0's representation can have it, and turning the switch off does not
    // cost it a row.
    let srv = Server::start();
    let mut c = srv.connect();
    assert_eq!(read_packed_rows(&mut c), "yes", "on by default since 5.4.1");

    load_rows(&mut c);
    let before = int(&cmd(&mut c, &[b"MEMORY", b"USAGE", b"row:5"]));
    assert_eq!(declare(&mut c), b"+OK\r\n");
    let packed = wait_shrunk(&mut c, b"row:5", before);
    assert!(packed < before, "the default packs without being asked");

    assert_eq!(cmd(&mut c, &[b"CONFIG", b"SET", b"packed-rows", b"no"]), b"+OK\r\n");
    assert_eq!(read_packed_rows(&mut c), "no");

    // Turning it off does not convert anything back — the form is a storage
    // decision, not a mode — so what has to hold is that every row still
    // answers, and that a write from here on lands in the general form.
    let last = format!("row:{}", ROWS - 1);
    assert_eq!(cmd(&mut c, &[b"HLEN", last.as_bytes()]), b":6\r\n");
    assert_eq!(
        cmd(&mut c, &[b"HGET", last.as_bytes(), b"name"]),
        format!("${}\r\nuser{}\r\n", 4 + (ROWS - 1).to_string().len(), ROWS - 1).into_bytes()
    );
    cmd(&mut c, &[b"HSET", b"row:5", b"name", b"renamed"]);
    assert_eq!(cmd(&mut c, &[b"HGET", b"row:5", b"name"]), b"$7\r\nrenamed\r\n");
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

/// Rows that arrived through the snapshot loader must pack.
///
/// The loader installs rows through `Store::load_hash`, which does not go
/// through the dispatcher and therefore not through the write hook — unlike
/// AOF replay, which does. So these rows have never been offered to the
/// hook in their lives, which is what separates this from the test above.
///
/// The snapshot carries the table catalog beside the rows, so the restarted
/// server has the table before anyone declares it, and the backfill repacks
/// what the loader installed.
#[test]
fn a_snapshot_restore_comes_back_packed() {
    let dir = std::env::temp_dir().join(format!("kevy-packbf-snap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let packed_before = {
        let srv = Server::start_in(&dir);
        let mut c = srv.connect();
        load_rows(&mut c);
        assert_eq!(declare(&mut c), b"+OK\r\n");
        let before = int(&cmd(&mut c, &[b"MEMORY", b"USAGE", b"row:5"]));
        let packed = wait_shrunk(&mut c, b"row:5", before);
        assert!(packed < before, "setup: the row must be packed before the restart");
        assert_eq!(cmd(&mut c, &[b"SAVE"]), b"+OK\r\n");
        packed
    };

    let srv = Server::start_in(&dir);
    let mut c = srv.connect();
    // The switch does NOT survive: CONFIG SET writes the running config, not
    // the file. Asserted from the other side now that the default is on — a
    // server told at runtime to STOP packing comes back packing. It is worth
    // its own assertion because when it bit, it looked exactly like "the
    // backfill missed the snapshot".
    assert_eq!(read_packed_rows(&mut c), "yes", "CONFIG SET does not outlive the process");
    assert_eq!(cmd(&mut c, &[b"HGET", b"row:5", b"name"]), b"$5\r\nuser5\r\n");

    // The snapshot carried the table catalog, so nobody declares anything:
    // the backfill finds the rows the loader installed in the general form
    // and packs them again.
    let tables = cmd(&mut c, &[b"TABLE.LIST"]);
    assert!(tables.starts_with(b"*1\r\n"), "the snapshot carried the table");
    let deadline =
        std::time::Instant::now() + kevy_testnet::patience(std::time::Duration::from_secs(4));
    let mut repacked = int(&cmd(&mut c, &[b"MEMORY", b"USAGE", b"row:5"]));
    while repacked != packed_before && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
        repacked = int(&cmd(&mut c, &[b"MEMORY", b"USAGE", b"row:5"]));
    }
    assert_eq!(repacked, packed_before, "the snapshot's rows cost what they did before");
    assert_eq!(cmd(&mut c, &[b"HLEN", b"row:5"]), b":6\r\n");
    let _ = std::fs::remove_dir_all(&dir);
}

//! `kevy-cli sql compile [--apply]` end-to-end: compile a real schema,
//! apply it against a spawned kevy server through the actual binary,
//! then run a query card with real arguments — the full declaration →
//! runtime story the cookbook chapter tells.

use std::process::{Child, Command, Stdio};

use kevy_resp_client::RespClient;

const SCHEMA: &str = r"
CREATE TABLE users (
  id     bigserial PRIMARY KEY,
  email  text,
  name   text
);
CREATE UNIQUE INDEX ON users (email);

CREATE TABLE orders (
  id          bigserial PRIMARY KEY,
  user_id     bigint,
  status      text,
  total       numeric(10,2),
  created_at  bigint
);
CREATE INDEX ON orders (status) INCLUDE (total, created_at);
CREATE INDEX ON orders (user_id, created_at DESC);

CREATE VIEW paid_orders AS
  SELECT * FROM orders WHERE status = 'paid';

CREATE VIEW recent_orders_by_user AS
  SELECT id, status, total FROM orders
  WHERE user_id = $1
  ORDER BY created_at DESC
  LIMIT 20;
";

struct Srv {
    child: Child,
    port: u16,
    /// Throwaway `--dir` — the catalog sidecars (index/view/table)
    /// persist there; without it a spawned server would write them
    /// into the crate directory and leak state across test runs.
    dir: std::path::PathBuf,
}

impl Srv {
    fn start(tag: &str) -> Srv {
        let port =
            std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let bin = std::path::Path::new(env!("CARGO_BIN_EXE_kevy-cli"))
            .parent()
            .unwrap()
            .join("kevy");
        if !bin.exists() {
            // cargo doesn't know this test depends on the kevy bin;
            // build it deterministically (no-op when fresh).
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = Command::new(cargo)
                .args(["build", "-p", "kevy", "--bin", "kevy"])
                .status()
                .expect("spawn cargo build");
            assert!(status.success(), "cargo build -p kevy --bin kevy failed");
        }
        let dir =
            std::env::temp_dir().join(format!("kevy-sqlcli-srv-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let child = Command::new(&bin)
            .args(["--port", &port.to_string(), "--threads", "2", "--no-aof"])
            .arg("--dir")
            .arg(&dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kevy server");
        for _ in 0..500 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Srv { child, port, dir }
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn cli(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_kevy-cli"))
        .args(args)
        .output()
        .expect("run kevy-cli")
}

fn write_schema(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("kevy-sql-cli-{tag}-{}.sql", std::process::id()));
    std::fs::write(&p, SCHEMA).unwrap();
    p
}

#[test]
fn compile_prints_the_script() {
    let f = write_schema("print");
    let out = cli(&["sql", "compile", f.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8(out.stdout).unwrap();
    assert!(s.contains("TABLE.DECLARE users PREFIX users: PK id"), "{s}");
    assert!(s.contains("VIEW.CREATE paid_orders"), "{s}");
    assert!(s.contains("#@card recent_orders_by_user"), "{s}");
    let _ = std::fs::remove_file(f);
}

#[test]
fn compile_error_is_the_teaching_refusal() {
    let p = std::env::temp_dir().join(format!("kevy-sql-cli-bad-{}.sql", std::process::id()));
    std::fs::write(&p, "CREATE VIEW v AS SELECT * FROM a JOIN b ON a.x = b.x;").unwrap();
    let out = cli(&["sql", "compile", p.to_str().unwrap()]);
    assert!(!out.status.success());
    let e = String::from_utf8(out.stderr).unwrap();
    assert!(e.contains("JOIN is not compilable"), "{e}");
    assert!(e.contains("line 1"), "{e}");
    let _ = std::fs::remove_file(p);
}

#[test]
fn apply_declares_then_a_card_query_serves() {
    let srv = Srv::start("serve");
    let f = write_schema("serve");
    let url = format!("127.0.0.1:{}", srv.port);
    let out = cli(&["sql", "compile", f.to_str().unwrap(), "--apply", "--url", &url]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8(out.stdout).unwrap();
    assert!(s.contains("TABLE.DECLARE users \u{2192} OK"), "{s}");
    assert!(s.contains("TABLE.DECLARE orders \u{2192} OK"), "{s}");
    assert!(s.contains("VIEW.CREATE paid_orders \u{2192} OK"), "{s}");
    assert!(s.contains("query card(s) are runtime templates"), "{s}");

    // Write rows, then run the compiled card with a real argument.
    let mut c = RespClient::connect("127.0.0.1", srv.port).unwrap();
    let hset = |c: &mut RespClient, k: &str, kv: &[(&str, &str)]| {
        let mut argv: Vec<Vec<u8>> = vec![b"HSET".to_vec(), k.as_bytes().to_vec()];
        for (f, v) in kv {
            argv.push(f.as_bytes().to_vec());
            argv.push(v.as_bytes().to_vec());
        }
        c.request(&argv).unwrap();
    };
    hset(&mut c, "orders:1", &[("id", "1"), ("user_id", "42"), ("status", "paid"), ("total", "19.5"), ("created_at", "100")]);
    hset(&mut c, "orders:2", &[("id", "2"), ("user_id", "42"), ("status", "pending"), ("total", "5"), ("created_at", "200")]);
    hset(&mut c, "orders:3", &[("id", "3"), ("user_id", "7"), ("status", "paid"), ("total", "8"), ("created_at", "300")]);

    // The card: IDX.QUERY orders.user_id_created_at WHERE user_id EQ $1 …
    // ($1 = 42). Index backfill is async — poll past -INDEXBUILDING.
    let card: Vec<Vec<u8>> = ["IDX.QUERY", "orders.user_id_created_at", "WHERE", "user_id", "EQ", "42", "LIMIT", "20"]
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
    let keys = poll_keys(&mut c, &card);
    // created_at DESC: orders:2 (200) before orders:1 (100).
    assert_eq!(keys, vec!["orders:2".to_string(), "orders:1".to_string()]);

    // The constant view serves too.
    let vq: Vec<Vec<u8>> =
        ["VIEW.QUERY", "paid_orders", "LIMIT", "10"].iter().map(|s| s.as_bytes().to_vec()).collect();
    let keys = poll_keys(&mut c, &vq);
    assert_eq!(keys, vec!["orders:1".to_string(), "orders:3".to_string()]);
    let _ = std::fs::remove_file(f);
}

/// Run a query until the async backfill finishes; return the hit keys.
fn poll_keys(c: &mut RespClient, argv: &[Vec<u8>]) -> Vec<String> {
    for _ in 0..200 {
        match c.request(argv).unwrap() {
            kevy_cli::Reply::Error(e) => {
                let e = String::from_utf8_lossy(&e).into_owned();
                assert!(e.contains("INDEXBUILDING"), "unexpected error: {e}");
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
            // Scalar page shape: `[cursor, [k, v, k, v…]]`.
            kevy_cli::Reply::Array(items) => {
                let inner = items
                    .iter()
                    .find_map(|it| match it {
                        kevy_cli::Reply::Array(kv) => Some(kv.as_slice()),
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("no page array in reply: {items:?}"));
                return inner
                    .chunks(2)
                    .filter_map(|pair| match pair.first() {
                        Some(kevy_cli::Reply::Bulk(b)) => {
                            Some(String::from_utf8_lossy(b).into_owned())
                        }
                        _ => None,
                    })
                    .collect();
            }
            other => panic!("unexpected reply: {other:?}"),
        }
    }
    panic!("index never finished building");
}

#[test]
fn apply_stops_on_error_reply_nonzero_exit() {
    let srv = Srv::start("twice");
    let f = write_schema("twice");
    let url = format!("127.0.0.1:{}", srv.port);
    let ok = cli(&["sql", "compile", f.to_str().unwrap(), "--apply", "--url", &url]);
    assert!(ok.status.success());
    // A second apply re-declares: the first TABLE.DECLARE errors
    // (table exists), the apply stops there, exit is non-zero.
    let again = cli(&["sql", "compile", f.to_str().unwrap(), "--apply", "--url", &url]);
    assert!(!again.status.success());
    let e = String::from_utf8(again.stderr).unwrap();
    assert!(e.contains("apply stopped at the error above"), "{e}");
    let _ = std::fs::remove_file(f);
}

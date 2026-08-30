//! `lint` against a real server: the two verdicts, which are the
//! contract a declaring script consumes.
//!
//! The arithmetic behind them is pinned without a server in
//! `kevy_cli::lint`'s own tests. What needs a server is the reading —
//! and the deliberate difference between the two exit codes.

use std::process::{Child, Command};

use kevy_resp_client::RespClient;

struct Srv {
    child: Child,
    port: u16,
    /// The server's data directory. Without one it writes its catalog
    /// and AOF into the test binary's cwd — which is this crate's
    /// source directory, where the files then survive into the next
    /// run and answer "table already exists".
    dir: std::path::PathBuf,
}

impl Srv {
    fn start() -> Srv {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let bin =
            std::path::Path::new(env!("CARGO_BIN_EXE_kevy-cli")).parent().unwrap().join("kevy");
        if !bin.exists() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = Command::new(cargo)
                .args(["build", "-p", "kevy", "--bin", "kevy"])
                .status()
                .expect("spawn cargo build");
            assert!(status.success(), "cargo build -p kevy --bin kevy failed");
        }
        let dir = std::env::temp_dir().join(format!("kevy-srv-{port}"));
        std::fs::create_dir_all(&dir).unwrap();
        let child = Command::new(&bin)
            .args(["--port", &port.to_string(), "--threads", "1", "--no-aof"])
            .args(["--dir", dir.to_str().unwrap()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn kevy server");
        kevy_testnet::assert_listening(port, "the server under test");
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

fn lint(port: u16, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_kevy-cli"))
        .args(["lint"])
        .args(args)
        .args(["-p", &port.to_string()])
        .output()
        .expect("run kevy-cli");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// Overlap is an answer, not a hint: a name under two owners means no
/// column can carry the dimension, so the command fails and a
/// declaring script stops.
#[test]
fn overlap_fails_when_a_name_lives_under_two_owners() {
    let srv = Srv::start();
    let mut c = RespClient::connect("127.0.0.1", srv.port).unwrap();
    for (k, m) in [("mailbox:1", "t1"), ("mailbox:1", "t2"), ("mailbox:2", "t2")] {
        c.request_borrowed(&[b"ZADD", k.as_bytes(), b"1", m.as_bytes()]).unwrap();
    }
    // A sidecar under the same prefix: discovered, not named, so it is
    // skipped and counted rather than failing the whole command — real
    // keyspaces have counters sitting beside their collections.
    c.request_borrowed(&[b"SET", b"mailbox:count", b"2"]).unwrap();
    c.request_borrowed(&[b"SADD", b"owner:a", b"x"]).unwrap();
    c.request_borrowed(&[b"SADD", b"owner:b", b"y"]).unwrap();

    let (ok, text) = lint(srv.port, &["overlap", "--prefix", "mailbox:"]);
    assert!(!ok, "{text}");
    assert!(text.contains("t2"), "names the shared item:\n{text}");
    assert!(text.contains("membership row"), "names the shape:\n{text}");
    assert!(text.contains("1 key(s) under this prefix are not collections"), "{text}");
    assert!(text.contains("2 owner(s)"), "the sidecar is not counted as an owner:\n{text}");

    let (ok, text) = lint(srv.port, &["overlap", "--prefix", "owner:"]);
    assert!(ok, "{text}");
    assert!(text.contains("a column can carry this dimension"), "{text}");
}

/// Coincidence is a suspicion — two columns may legitimately agree —
/// so it reports and exits zero either way.
#[test]
fn copied_columns_are_reported_without_failing() {
    let srv = Srv::start();
    let mut c = RespClient::connect("127.0.0.1", srv.port).unwrap();
    // Exactly what `kevy-cli sql compile` emits for this schema — the
    // shape is the compiler's, not guessed here.
    let declared = c
        .request_borrowed(&[
            b"TABLE.DECLARE",
            b"ev",
            b"PREFIX",
            b"ev:",
            b"PK",
            b"id",
            b"COLUMN",
            b"id",
            b"i64",
            b"COLUMN",
            b"created_at",
            b"i64",
            b"COLUMN",
            b"sort_ts",
            b"i64",
        ])
        .unwrap();
    assert!(
        !matches!(declared, kevy_resp_client::Reply::Error(_)),
        "TABLE.DECLARE was refused: {declared:?}"
    );
    for i in 1..=10 {
        let key = format!("ev:{i}");
        let ts = format!("{}", 1000 + i);
        c.request_borrowed(&[
            b"HSET",
            key.as_bytes(),
            b"id",
            i.to_string().as_bytes(),
            b"created_at",
            ts.as_bytes(),
            b"sort_ts",
            ts.as_bytes(),
        ])
        .unwrap();
    }

    let (ok, text) = lint(srv.port, &["columns", "ev"]);
    assert!(ok, "a suspicion must not fail:\n{text}");
    assert!(text.contains("created_at and sort_ts"), "{text}");
    assert!(text.contains("ORDERPATH"), "points at the answer:\n{text}");

    let (ok, text) = lint(srv.port, &["columns", "nosuch"]);
    assert!(!ok, "an undeclared table is an error:\n{text}");
}

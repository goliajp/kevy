//! `backfill-keys` against a real server: the parts a unit test cannot
//! reach — the verb chosen from the key's type, SCAN paging over a
//! prefix, prefix stripping, and the split of names to stdout from
//! accounting to stderr.
//!
//! The arithmetic of "unique to this source" is pinned without a server
//! in `kevy_cli::backfill_keys`'s own tests; this is about the reading.

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
        let bin = std::path::Path::new(env!("CARGO_BIN_EXE_kevy-cli"))
            .parent()
            .unwrap()
            .join("kevy");
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

fn run(port: u16, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_kevy-cli"))
        .args(["backfill-keys", "-p", &port.to_string()])
        .args(args)
        .output()
        .expect("run kevy-cli");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Three source kinds that disagree, which is the case the lesson was
/// paid for: two names exist in exactly one source each, and reading
/// any single source would have missed them.
#[test]
fn the_union_spans_every_source_kind_and_names_what_only_one_had() {
    let srv = Srv::start();
    let mut c = RespClient::connect("127.0.0.1", srv.port).unwrap();
    for m in ["1", "2", "3"] {
        c.request_borrowed(&[b"ZADD", b"idx:a", b"1", m.as_bytes()]).unwrap();
    }
    // 9001 is outside the prefix range on purpose: it is the name
    // only this index knows.
    c.request_borrowed(&[b"SADD", b"idx:b", b"3", b"4", b"9001"]).unwrap();
    // More than one SCAN batch, so paging is actually exercised.
    for i in 1..=600 {
        let k = format!("mail:{i}");
        c.request_borrowed(&[b"SET", k.as_bytes(), b"v"]).unwrap();
    }
    let dir = std::env::temp_dir().join(format!("kevy-bfk-{}", srv.port));
    std::fs::create_dir_all(&dir).unwrap();
    let archive = dir.join("archive.txt");
    std::fs::write(&archive, "5\n9999\n").unwrap();

    let (ok, names, acct) = run(
        srv.port,
        &[
            "--from-index",
            "idx:a",
            "--from-index",
            "idx:b",
            "--from-prefix",
            "mail:",
            "--from-file",
            archive.to_str().unwrap(),
        ],
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert!(ok, "{acct}");

    let got: Vec<&str> = names.lines().collect();
    // 600 keys under the prefix, plus 9001 from idx:b and 9999 from the
    // archive; every other name is already one of the 600.
    assert_eq!(got.len(), 602, "union size\n{acct}");
    assert!(got.contains(&"9001") && got.contains(&"9999"), "{acct}");
    // Stripped, not whole keys: these line up with the index members.
    assert!(got.contains(&"600") && !got.contains(&"mail:600"), "{acct}");

    // The accounting rides stderr, so redirecting the names keeps it.
    assert!(acct.contains("602 name(s) in the union"), "{acct}");
    // The number that matters per source: idx:b and the archive each
    // hold exactly one name no other source could have named.
    assert!(acct.contains("3 name(s), 1 only here"), "idx:b\n{acct}");
    assert!(acct.contains("2 name(s), 1 only here"), "the archive\n{acct}");
    assert!(acct.contains("appear in only one source"), "{acct}");
    assert!(!names.contains("only one source"), "accounting leaked into stdout");
}

/// A source that cannot be read is an error, not an empty contribution
/// — a silently empty source is the hole this command exists to close.
#[test]
fn an_unreadable_source_fails_rather_than_contributing_nothing() {
    let srv = Srv::start();
    let mut c = RespClient::connect("127.0.0.1", srv.port).unwrap();
    c.request_borrowed(&[b"SET", b"astring", b"x"]).unwrap();

    let (ok, _, err) = run(srv.port, &["--from-index", "ghost"]);
    assert!(!ok);
    assert!(err.contains("does not exist"), "{err}");

    let (ok, _, err) = run(srv.port, &["--from-index", "astring"]);
    assert!(!ok);
    assert!(err.contains("is a string"), "{err}");
}

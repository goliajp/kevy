//! v2.0.16 — `CLIENT SETNAME` / `CLIENT GETNAME` per-conn persistence
//! (closes v1.52.x finding).
//!
//! Validates the reactor-level intercept in
//! `crates/kevy-rt/src/exec_client_intercept.rs`:
//! - `SETNAME` persists the name on the conn;
//! - `GETNAME` returns the persisted name (not the v1.x stub `$0`);
//! - Per-connection isolation (one conn's name doesn't leak to another);
//! - Invalid names (whitespace / control chars) rejected with -ERR;
//! - Empty `SETNAME` is allowed (clears the name);
//! - `CLIENT` LIST / INFO / KILL / NO-EVICT etc. still work via the
//!   standard dispatch (interception is SETNAME/GETNAME-only).
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test client_setname_persistence --release -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig, pick_free_port};

#[test]
#[ignore = "chaos test — opt-in via --ignored"]
fn client_setname_persists_per_connection() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-setname-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 1;
    let _h = Harness::spawn(cfg).expect("spawn kevy");
    std::thread::sleep(Duration::from_millis(200));

    // PHASE 1: Single-conn round-trip.
    let mut a = connect(port);
    let r = send(&mut a, b"*3\r\n$6\r\nCLIENT\r\n$7\r\nSETNAME\r\n$5\r\nconn1\r\n");
    assert!(r.starts_with("+OK"), "SETNAME got: {r:?}");
    let r = send(&mut a, b"*2\r\n$6\r\nCLIENT\r\n$7\r\nGETNAME\r\n");
    assert!(
        r == "$5\r\nconn1\r\n",
        "GETNAME round-trip failed: {r:?}"
    );

    // PHASE 2: Per-connection isolation. A second connection's
    // GETNAME must return empty bulk (not conn1's name).
    let mut b = connect(port);
    let r = send(&mut b, b"*2\r\n$6\r\nCLIENT\r\n$7\r\nGETNAME\r\n");
    assert_eq!(r, "$0\r\n\r\n", "fresh conn must have empty name, got {r:?}");

    // PHASE 3: Rename on a fresh conn (overwrite semantics).
    let mut a2 = connect(port);
    let r = send(&mut a2, b"*3\r\n$6\r\nCLIENT\r\n$7\r\nSETNAME\r\n$4\r\nfoo1\r\n");
    assert!(r.starts_with("+OK"));
    let r = send(&mut a2, b"*2\r\n$6\r\nCLIENT\r\n$7\r\nGETNAME\r\n");
    assert_eq!(r, "$4\r\nfoo1\r\n");
    let r = send(&mut a2, b"*3\r\n$6\r\nCLIENT\r\n$7\r\nSETNAME\r\n$4\r\nbar2\r\n");
    assert!(r.starts_with("+OK"));
    let r = send(&mut a2, b"*2\r\n$6\r\nCLIENT\r\n$7\r\nGETNAME\r\n");
    assert_eq!(r, "$4\r\nbar2\r\n", "rename should overwrite");

    // PHASE 4: Whitespace rejected per Redis spec.
    let mut c = connect(port);
    let r = send(&mut c, b"*3\r\n$6\r\nCLIENT\r\n$7\r\nSETNAME\r\n$6\r\nha ck1\r\n");
    assert!(
        r.starts_with("-ERR"),
        "SETNAME with whitespace should reject, got {r:?}"
    );
    // After reject, name stays empty.
    let r = send(&mut c, b"*2\r\n$6\r\nCLIENT\r\n$7\r\nGETNAME\r\n");
    assert_eq!(r, "$0\r\n\r\n");

    // PHASE 5: Empty SETNAME allowed — clears the name.
    let r = send(&mut a, b"*3\r\n$6\r\nCLIENT\r\n$7\r\nSETNAME\r\n$0\r\n\r\n");
    assert!(r.starts_with("+OK"));
    let r = send(&mut a, b"*2\r\n$6\r\nCLIENT\r\n$7\r\nGETNAME\r\n");
    assert_eq!(r, "$0\r\n\r\n");

    // PHASE 6: Other CLIENT subcommands still work via the standard
    // dispatch (interception is SETNAME/GETNAME-only).
    let r = send(&mut a, b"*2\r\n$6\r\nCLIENT\r\n$2\r\nID\r\n");
    assert!(r.starts_with(':'), "CLIENT ID still works: {r:?}");
    let r = send(&mut a, b"*3\r\n$6\r\nCLIENT\r\n$8\r\nNO-EVICT\r\n$2\r\nON\r\n");
    assert!(r.starts_with("+OK"), "CLIENT NO-EVICT still works: {r:?}");

    eprintln!("client_setname: 6 phases passed — per-conn name persists, isolated, validated");
    drop(a);
    drop(a2);
    drop(b);
    drop(c);
    let _ = std::fs::remove_dir_all(&tmp);
}

fn connect(port: u16) -> TcpStream {
    let s = TcpStream::connect(format!("127.0.0.1:{port}")).expect("conn");
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    s
}

fn send(s: &mut TcpStream, cmd: &[u8]) -> String {
    s.write_all(cmd).expect("write");
    let mut buf = vec![0u8; 256];
    let n = s.read(&mut buf).expect("read");
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

fn resolve_kevy_bin() -> PathBuf {
    if let Ok(p) = std::env::var("KEVY_BIN") {
        return PathBuf::from(p);
    }
    let here = std::env::current_dir().unwrap();
    let mut p = here.clone();
    loop {
        let candidate = p.join("target/release/kevy");
        if candidate.exists() {
            return candidate;
        }
        if !p.pop() {
            panic!(
                "kevy release binary not found above {}; run `cargo build --release -p kevy`",
                here.display()
            );
        }
    }
}

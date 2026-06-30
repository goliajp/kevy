//! v1.58 — verify v1.38.x finding fix: kevy survives `SIGXFSZ`
//! (write exceeds `RLIMIT_FSIZE`) without being kernel-killed.
//!
//! Before v1.58: kevy had no SIGXFSZ handler. Default action is
//! `Core` (kernel terminates + dumps core). One disk-full event
//! killed the whole server.
//!
//! v1.58: kevy installs a no-op `SIGXFSZ` handler. The signal is
//! absorbed, the failing write returns `EFBIG` to the AOF writer
//! (which logs + ignores via `eprintln!` per `exec.rs:319`), and
//! kevy keeps serving.
//!
//! This test verifies the handler's *core property* by sending
//! SIGXFSZ directly to the kevy process and asserting it survives.
//! That decouples the test from kevy's specific AOF write path
//! (io_uring vs std::fs) and just proves the signal handler does
//! its job.
//!
//! Strict asserts:
//! - Spawn kevy.
//! - Send `SIGXFSZ` (signal 25) to its PID via `kill -25 <pid>`.
//!   Pre-v1.58 this would kernel-kill the process.
//! - kevy answers PING after the signal (proves it survived).
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test sigxfsz_survival_chaos --release -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig, pick_free_port};

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn sigxfsz_does_not_kill_kevy() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-xfsz-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 1;
    let h = Harness::spawn(cfg).expect("spawn kevy");
    std::thread::sleep(Duration::from_millis(200));

    // PHASE 1: prove kevy is up.
    {
        let mut s = TcpStream::connect(format!("127.0.0.1:{port}"))
            .expect("pre-signal conn");
        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
        s.write_all(b"*1\r\n$4\r\nPING\r\n").expect("pre-PING write");
        let mut buf = [0u8; 32];
        let n = s.read(&mut buf).expect("pre-PING read");
        assert!(buf[..n].starts_with(b"+PONG"), "pre-signal PING failed");
    }

    // PHASE 2: send SIGXFSZ directly. We get the child PID via the
    // harness's listening port — `lsof -ti :<port>` returns the pid.
    let pid = find_pid_on_port(port).expect("locate kevy pid");
    eprintln!("xfsz: located kevy pid = {pid}, sending SIGXFSZ");
    let status = Command::new("kill")
        .args(["-25", &pid.to_string()])
        .status()
        .expect("kill");
    assert!(status.success(), "kill -25 {pid} failed");

    // Give kevy a moment to either die (pre-v1.58) or absorb (v1.58).
    std::thread::sleep(Duration::from_millis(200));

    // PHASE 3: post-signal PING — kevy MUST still answer.
    eprintln!("xfsz: post-SIGXFSZ PING");
    let mut ping = TcpStream::connect(format!("127.0.0.1:{port}"))
        .expect("post-xfsz conn — kevy was kernel-killed (no v1.58 handler?)");
    let _ = ping.set_read_timeout(Some(Duration::from_secs(2)));
    ping.write_all(b"*1\r\n$4\r\nPING\r\n").expect("post-PING write");
    let mut buf = [0u8; 32];
    let n = ping.read(&mut buf).expect("post-PING read");
    assert!(
        buf[..n].starts_with(b"+PONG"),
        "post-SIGXFSZ PING failed: {:?}",
        String::from_utf8_lossy(&buf[..n])
    );
    eprintln!("xfsz: kevy alive after SIGXFSZ — handler absorbed the signal");

    drop(ping);
    drop(h);
    let _ = std::fs::remove_dir_all(&tmp);
}

fn find_pid_on_port(port: u16) -> Option<u32> {
    let out = Command::new("lsof")
        .args(["-ti", &format!(":{port}")])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines().next().and_then(|l| l.trim().parse().ok())
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
                "kevy release binary not found above {}; run `cargo build --release -p kevy` first or set KEVY_BIN",
                here.display()
            );
        }
    }
}

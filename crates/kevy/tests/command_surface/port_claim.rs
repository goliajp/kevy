//! A second server on a port another server holds must refuse to start.
//! Each shard listens with SO_REUSEPORT, which on its own would let the
//! second process join the first one's listeners and take half its traffic.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn a_second_server_on_a_held_port_refuses_to_start() {
    let port = kevy_testnet::free_port();
    let dirs = [kevy_tmpdir::TmpDir::new("port-claim-a"), kevy_tmpdir::TmpDir::new("port-claim-b")];
    let spawn = |dir: &kevy_tmpdir::TmpDir| {
        Command::new(env!("CARGO_BIN_EXE_kevy"))
            .args(["--port", &port.to_string(), "--threads", "2", "--no-aof", "--dir"])
            .arg(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kevy")
    };
    let mut first = spawn(&dirs[0]);
    kevy_testnet::assert_listening(port, "the first server");
    let mut second = spawn(&dirs[1]);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = second.try_wait().unwrap() {
            break Some(s);
        }
        if Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let _ = second.kill();
    let out = second.wait_with_output().unwrap();
    let _ = first.kill();
    let _ = first.wait();
    let status = status.expect("the second server kept running on a held port");
    assert!(!status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.to_lowercase().contains("in use"), "{err}");
}

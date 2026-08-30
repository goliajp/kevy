//! `KEVY_UNIX_SOCKET` must be *served*, not merely bound — on the
//! readiness reactor as well as io_uring.
//!
//! The listener is bound in `Runtime::run` before any shard spawns, so
//! the socket file appears and `connect()` succeeds into the kernel
//! backlog whatever the reactor does afterwards. Only the io_uring
//! reactor ever accepted on it; on kqueue and on the epoll fallback the
//! client waited forever for a reply that no one was going to send.
//!
//! So this test waits for a **reply**, never for the file. A test that
//! asserted the socket exists would have passed against the version
//! that hangs.
//!
//! `KEVY_IO_URING=0` is what makes it a regression test rather than a
//! coincidence: on Linux the default reactor is io_uring, which already
//! worked, and the broken path would never be reached.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    dir: std::path::PathBuf,
    sock: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start() -> Server {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let dir = std::env::temp_dir().join(format!("kevy-uds-{port}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // kevy refuses to start on a pre-existing socket path, so keep it
    // inside the per-run directory we just made.
    let sock = dir.join("kevy.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_kevy"))
        .args(["--port", &port.to_string(), "--threads", "1", "--no-aof"])
        .args(["--dir", dir.to_str().unwrap()])
        .env("KEVY_UNIX_SOCKET", &sock)
        .env("KEVY_IO_URING", "0")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn kevy");
    Server { child, dir, sock }
}

/// Connect and speak, retrying until something answers or the budget
/// runs out. The budget is generous because the workspace suite shares
/// the machine; what it must never do is succeed on silence.
fn talk(sock: &std::path::Path, request: &[u8], want_bytes: usize) -> Option<Vec<u8>> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(mut s) = UnixStream::connect(sock) {
            s.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
            if s.write_all(request).is_ok() {
                let mut got = Vec::new();
                let mut buf = [0u8; 4096];
                let until = Instant::now() + Duration::from_secs(2);
                while got.len() < want_bytes && Instant::now() < until {
                    match s.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                        Err(_) => break,
                    }
                }
                if !got.is_empty() {
                    return Some(got);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

#[test]
fn the_unix_socket_answers_on_the_readiness_reactor() {
    let srv = start();
    let got = talk(&srv.sock, b"PING\r\n", 7)
        .expect("the unix socket never answered — it is bound but nothing accepts on this reactor");
    assert_eq!(got, b"+PONG\r\n");
}

#[test]
fn a_write_through_the_unix_socket_reads_back() {
    let srv = start();
    let got = talk(&srv.sock, b"SET uk via-uds\r\nGET uk\r\n", 18)
        .expect("the unix socket never answered");
    assert_eq!(got, b"+OK\r\n$7\r\nvia-uds\r\n", "got {:?}", String::from_utf8_lossy(&got));
}

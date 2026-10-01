//! A command whose key is not its first argument routes by that key, also
//! inside a pipeline, where the server works out each command's route
//! while the one before it runs.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;

fn encode(cmd: &[&str]) -> String {
    let mut req = format!("*{}\r\n", cmd.len());
    for a in cmd {
        req.push_str(&format!("${}\r\n{a}\r\n", a.len()));
    }
    req
}

#[test]
fn xgroup_create_routes_by_its_key_in_a_pipeline() {
    let port = kevy_testnet::free_port();
    let dir = kevy_tmpdir::TmpDir::new("key-beyond-first-arg");
    let mut server = Command::new(env!("CARGO_BIN_EXE_kevy"))
        .args(["--port", &port.to_string(), "--threads", "4", "--no-aof", "--dir"])
        .arg(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kevy");
    kevy_testnet::assert_listening(port, "kevy");
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    // one write: every XGROUP's key sits at argv[2], argv[1] is "CREATE"
    let mut req = String::new();
    for i in 0..20 {
        req += &encode(&["XGROUP", "CREATE", &format!("s{i}"), "g", "$", "MKSTREAM"]);
        req += &encode(&["EXISTS", &format!("s{i}")]);
    }
    s.write_all(req.as_bytes()).unwrap();
    let mut r = BufReader::new(s);
    let mut replies = Vec::new();
    for _ in 0..40 {
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        replies.push(line);
    }
    let _ = server.kill();
    let _ = server.wait();
    let want: Vec<&str> = (0..20).flat_map(|_| ["+OK\r\n", ":1\r\n"]).collect();
    assert_eq!(replies, want);
}

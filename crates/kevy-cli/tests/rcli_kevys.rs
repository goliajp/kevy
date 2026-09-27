//! `kevy-cli -u kevys://…` against a real kevy with its encrypted client
//! port open and one client key listed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

struct Srv {
    child: Child,
    secure_port: u16,
    server_pub: String,
    client_key: PathBuf,
    dir: PathBuf,
}

fn kevy_bin() -> PathBuf {
    let bin = Path::new(env!("CARGO_BIN_EXE_kevy-cli")).parent().unwrap().join("kevy");
    if !bin.exists() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "-p", "kevy", "--bin", "kevy"])
            .status()
            .expect("spawn cargo build");
        assert!(status.success(), "cargo build -p kevy --bin kevy failed");
    }
    bin
}

/// `kevy keygen <file>`: writes the private key, prints the public one.
fn keygen(bin: &Path, file: &Path) -> String {
    let out = Command::new(bin).arg("keygen").arg(file).output().expect("run kevy keygen");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

impl Srv {
    fn start() -> Srv {
        let bin = kevy_bin();
        let (port, secure_port) = (kevy_testnet::free_port(), kevy_testnet::free_port());
        let dir = std::env::temp_dir().join(format!("kevy-rcli-kevys-{port}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let server_pub = keygen(&bin, &dir.join("server.key"));
        let client_key = dir.join("client.key");
        let client_pub = keygen(&bin, &client_key);
        let conf = dir.join("kevy.toml");
        std::fs::write(
            &conf,
            format!(
                "[secure]\nprivate_key_file = \"{}\"\nlisten_port = {secure_port}\nclient_keys = [\"{client_pub}\"]\n",
                dir.join("server.key").display()
            ),
        )
        .unwrap();
        let child = Command::new(&bin)
            .args(["--config", conf.to_str().unwrap()])
            .args(["--port", &port.to_string(), "--threads", "1", "--no-aof"])
            .args(["--dir", dir.join("data").to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kevy server");
        kevy_testnet::assert_listening(secure_port, "the encrypted client port");
        Srv { child, secure_port, server_pub, client_key, dir }
    }

    fn url(&self, key: Option<&Path>) -> String {
        let mut u =
            format!("kevys://127.0.0.1:{}?server_key={}", self.secure_port, self.server_pub);
        if let Some(k) = key {
            u.push_str(&format!("&client_key_file={}", k.display()));
        }
        u
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        let pid = self.child.id().to_string();
        let _ = Command::new("kill").args(["-TERM", &pid]).status(); // may have exited already
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while matches!(self.child.try_wait(), Ok(None)) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

fn cli(args: &[&str], stdin: &[u8]) -> Out {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kevy-cli"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run kevy-cli");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    Out {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().unwrap_or(-1),
    }
}

#[test]
fn commands_and_a_large_value_go_through_the_encrypted_port() {
    let s = Srv::start();
    let url = s.url(Some(&s.client_key));
    assert_eq!(cli(&["-u", &url, "SET", "k", "a b"], b"").stdout, "OK\n");
    assert_eq!(cli(&["-u", &url, "GET", "k"], b"").stdout, "a b\n");
    // larger than one read of the CLI's buffer, so decrypted bytes wait in
    // the stream and must be taken before the reply parses
    let big = "v".repeat(50_000);
    assert_eq!(cli(&["-u", &url, "SET", "big", &big], b"").stdout, "OK\n");
    let got = cli(&["-u", &url, "GET", "big"], b"");
    assert_eq!(got.stdout.trim_end().len(), big.len(), "{}", got.stderr);
    // a db in the URL is selected
    let db0 = url.replacen('?', "/0?", 1);
    assert_eq!(cli(&["-u", &db0, "GET", "k"], b"").stdout, "a b\n");
}

#[test]
fn pipe_mode_writes_through_the_encrypted_session() {
    let s = Srv::start();
    let url = s.url(Some(&s.client_key));
    let mut input = Vec::new();
    for i in 0..2000 {
        input.extend(format!("*3\r\n$3\r\nSET\r\n$5\r\np{i:04}\r\n$1\r\nx\r\n").as_bytes());
    }
    let out = cli(&["-u", &url, "--pipe"], &input);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.contains("errors: 0, replies: 2000"), "{}", out.stdout);
    assert_eq!(cli(&["-u", &url, "DBSIZE"], b"").stdout, "2000\n");
}

#[test]
fn a_missing_client_key_a_wrong_server_key_and_a_bad_url_are_refused() {
    let s = Srv::start();
    let unlisted = cli(&["-u", &s.url(None), "PING"], b"");
    assert_ne!(unlisted.code, 0);
    assert!(unlisted.stderr.contains("Could not connect"), "{}", unlisted.stderr);

    let wrong = format!(
        "kevys://127.0.0.1:{}?server_key={}&client_key_file={}",
        s.secure_port,
        "ab".repeat(32),
        s.client_key.display()
    );
    assert_ne!(cli(&["-u", &wrong, "PING"], b"").code, 0);

    let bad = cli(&["-u", &format!("kevys://127.0.0.1:{}", s.secure_port), "PING"], b"");
    assert_eq!(bad.code, 1);
    assert!(bad.stderr.contains("server_key"), "{}", bad.stderr);
}

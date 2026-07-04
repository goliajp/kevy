//! v2.10 — export → import round-trip against two real servers.

use std::process::{Child, Command};

use kevy_cli::migrate::{run_export, run_import};
use kevy_resp_client::RespClient;

struct Srv {
    child: Child,
    port: u16,
}

impl Srv {
    fn start() -> Srv {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        // the kevy server binary lives next to our own test artifacts
        let bin = std::path::Path::new(env!("CARGO_BIN_EXE_kevy-cli"))
            .parent()
            .unwrap()
            .join("kevy");
        let child = Command::new(&bin)
            .args(["--port", &port.to_string(), "--threads", "2", "--no-aof"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn kevy server (cargo build -p kevy --bin kevy first)");
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Srv { child, port }
    }
    fn client(&self) -> RespClient {
        RespClient::connect("127.0.0.1", self.port).unwrap()
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn digest(c: &mut RespClient, prefix: &str) -> String {
    let r = c.request_borrowed(&[b"PREFIX.DIGEST", prefix.as_bytes()]).unwrap();
    format!("{r:?}")
}

#[test]
fn export_import_roundtrip_digest_equal() {
    let src = Srv::start();
    let dst = Srv::start();
    let mut cs = src.client();
    for i in 0..500 {
        cs.request_borrowed(&[b"SET", format!("mig:s:{i}").as_bytes(), format!("v{i}").as_bytes()]).unwrap();
        cs.request_borrowed(&[
            b"HSET", format!("mig:h:{i}").as_bytes(), b"a", format!("{i}").as_bytes(), b"b", b"x",
        ]).unwrap();
    }
    cs.request_borrowed(&[b"RPUSH", b"mig:list", b"1", b"2", b"3"]).unwrap();
    cs.request_borrowed(&[b"SADD", b"mig:set", b"p", b"q"]).unwrap();
    cs.request_borrowed(&[b"ZADD", b"mig:zset", b"1.5", b"m", b"2.5", b"n"]).unwrap();
    cs.request_borrowed(&[b"SET", b"mig:ttl", b"soon"]).unwrap();
    cs.request_borrowed(&[b"PEXPIRE", b"mig:ttl", b"60000"]).unwrap();
    cs.request_borrowed(&[b"SET", b"nomig:1", b"skip"]).unwrap();

    let dir = std::env::temp_dir().join(format!("kevy-mig-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("dump.resp");
    let n = run_export(&mut cs, Some(b"mig:"), &file).unwrap();
    assert_eq!(n, 1004, "500+500 rows + list/set/zset/ttl");

    let mut cd = dst.client();
    let rep = run_import(&mut cd, &file, false, true).unwrap();
    assert_eq!(rep.errors, 0);
    assert!(rep.sent >= 1004, "at least one frame per key: {}", rep.sent);

    let ds = digest(&mut cs, "mig:");
    let dd = digest(&mut cd, "mig:");
    assert_eq!(ds, dd, "src {ds} vs dst {dd}");
    let r = cd.request_borrowed(&[b"EXISTS", b"nomig:1"]).unwrap();
    assert_eq!(format!("{r:?}"), "Int(0)");
    let r = cd.request_borrowed(&[b"PTTL", b"mig:ttl"]).unwrap();
    let s = format!("{r:?}");
    let ms: i64 = s.trim_start_matches("Int(").trim_end_matches(')').parse().unwrap();
    assert!(ms > 30_000 && ms <= 60_000, "absolute TTL carried: {ms}");

    // resume: restart from a zeroed progress file → idempotent, digest equal
    let file2 = dir.join("dump2.resp");
    let n2 = run_export(&mut cs, Some(b"mig:"), &file2).unwrap();
    assert_eq!(n2, n);
    std::fs::write(file2.with_extension("progress"), b"0").unwrap();
    let rep2 = run_import(&mut cd, &file2, true, true).unwrap();
    assert_eq!(rep2.errors, 0, "idempotent replay");
    assert_eq!(digest(&mut cd, "mig:"), ds);
    let _ = std::fs::remove_dir_all(&dir);
}

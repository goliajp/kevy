//! FAILOVER follows the target's real replication port.
//!
//! Both nodes run with a non-default `[replication].listen_port_base`.
//! After `FAILOVER` the old primary must be replicating from the base
//! the target actually listens on, not from `client port + 10000`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use kevy_chaos::{Harness, HarnessConfig, pick_free_port};

fn spawn(port: u16, toml: String) -> (Harness, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("kevy-failover-base-{port}"));
    let _ = std::fs::remove_dir_all(&dir);
    let cfg = HarnessConfig {
        kevy_bin: std::path::PathBuf::from(env!("CARGO_BIN_EXE_kevy")),
        threads: 1,
        ..HarnessConfig::new(dir.clone(), port).with_fsync("everysec").with_extra_toml(toml)
    };
    (Harness::spawn(cfg).expect("spawn kevy"), dir)
}

fn command(port: u16, argv: &[&str]) -> Vec<u8> {
    let s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut w = s.try_clone().unwrap();
    let mut req = format!("*{}\r\n", argv.len());
    for a in argv {
        req.push_str(&format!("${}\r\n{a}\r\n", a.len()));
    }
    w.write_all(req.as_bytes()).unwrap();
    let mut r = BufReader::new(s);
    let mut head = String::new();
    r.read_line(&mut head).unwrap();
    let Some(len) = head.strip_prefix('$') else {
        return head.into_bytes();
    };
    let len: usize = len.trim().parse().unwrap();
    let mut body = vec![0u8; len + 2];
    r.read_exact(&mut body).unwrap();
    body.truncate(len);
    body
}

fn info_field(port: u16, field: &str) -> Option<String> {
    let text = String::from_utf8(command(port, &["INFO", "replication"])).unwrap();
    text.lines().find_map(|l| l.strip_prefix(&format!("{field}:")).map(|v| v.trim().to_string()))
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn failover_retargets_at_the_targets_configured_replication_base() {
    let p_port = pick_free_port();
    let p_repl = pick_free_port();
    let r_port = pick_free_port();
    let r_repl = pick_free_port();
    assert_ne!(r_repl, r_port + 10_000, "the test needs a non-default base");

    let (_p, _pd) =
        spawn(p_port, format!("[replication]\nrole = \"primary\"\nlisten_port_base = {p_repl}\n"));
    let (_r, _rd) = spawn(
        r_port,
        format!(
            "[replication]\nrole = \"replica\"\nupstream = \"127.0.0.1:{p_repl}\"\n\
             listen_port_base = {r_repl}\n"
        ),
    );
    wait_for("the replica link", || {
        info_field(r_port, "master_link_status").as_deref() == Some("up")
    });
    assert_eq!(info_field(r_port, "repl_port_base"), Some(r_repl.to_string()));

    let reply = command(p_port, &["FAILOVER", "127.0.0.1", &r_port.to_string()]);
    assert_eq!(reply, b"+OK\r\n");

    wait_for("the old primary to follow the target", || {
        info_field(p_port, "role").as_deref() == Some("slave")
            && info_field(p_port, "master_link_status").as_deref() == Some("up")
    });
    assert_eq!(info_field(p_port, "master_port"), Some(r_repl.to_string()));
}

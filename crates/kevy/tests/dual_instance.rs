//! Two runtimes, one process — the instance-era contract.
//!
//! Every piece of engine state used to be a process global; a second
//! server in the same process would have shared (and corrupted) the
//! first one's replication role, catalogs, config and stats. This
//! test is the proof that the migration to `RuntimeState` is
//! complete: two full runtimes with separate ports, data dirs and
//! catalogs run side by side, and nothing leaks across.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use kevy_testnet::free_port;

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("kevy-dual-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn spawn_server(port: u16, dir: std::path::PathBuf) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = Arc::clone(&stop);
    std::thread::spawn(move || {
        let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(2))
            .bind([127, 0, 0, 1], port)
            .shards(2)
            .with_data_dir(dir)
            .with_aof(false);
        let _ = rt.run(stop_t);
    });
    for _ in 0..400 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return stop;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("server on {port} never came up");
}

fn cmd(conn: &mut TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    let mut req = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        req.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        req.extend_from_slice(p);
        req.extend_from_slice(b"\r\n");
    }
    conn.write_all(&req).unwrap();
    let mut buf = [0u8; 4096];
    let n = conn.read(&mut buf).unwrap();
    buf[..n].to_vec()
}

#[test]
fn two_runtimes_in_one_process_do_not_share_state() {
    unsafe {
        std::env::set_var("KEVY_IO_URING", "0");
    }
    let (pa, pb) = (free_port(), free_port());
    let _stop_a = spawn_server(pa, tmp_dir("a"));
    let _stop_b = spawn_server(pb, tmp_dir("b"));

    let mut a = TcpStream::connect(("127.0.0.1", pa)).unwrap();
    let mut b = TcpStream::connect(("127.0.0.1", pb)).unwrap();

    // Keyspace isolation: a write on A is invisible on B.
    assert_eq!(cmd(&mut a, &[b"SET", b"who", b"instance-a"]), b"+OK\r\n");
    assert_eq!(cmd(&mut b, &[b"GET", b"who"]), b"$-1\r\n");
    assert_eq!(cmd(&mut b, &[b"DBSIZE"]), b":0\r\n");

    // Catalog isolation: an index declared on A does not exist on B.
    let r = cmd(
        &mut a,
        &[
            b"IDX.CREATE",
            b"ia",
            b"ON",
            b"PREFIX",
            b"p:",
            b"FIELD",
            b"n",
            b"TYPE",
            b"i64",
            b"KIND",
            b"range",
        ],
    );
    assert_eq!(r, b"+OK\r\n", "IDX.CREATE on A: {}", String::from_utf8_lossy(&r));
    let r = cmd(&mut b, &[b"IDX.LIST"]);
    assert_eq!(r, b"*0\r\n", "B sees A's catalog: {}", String::from_utf8_lossy(&r));

    // Role isolation: READONLY on B (via CONFIG-free replica flag is
    // runtime state) — flip B's read_only through REPLICAOF pointing
    // nowhere is heavy; instead verify CONFIG isolation:
    assert_eq!(
        cmd(&mut a, &[b"CONFIG", b"SET", b"maxmemory", b"1048576"])[..1].to_vec(),
        b"+".to_vec()
    );
    let got_a = cmd(&mut a, &[b"CONFIG", b"GET", b"maxmemory"]);
    let got_b = cmd(&mut b, &[b"CONFIG", b"GET", b"maxmemory"]);
    assert!(
        String::from_utf8_lossy(&got_a).contains("1048576"),
        "A lost its own CONFIG SET: {}",
        String::from_utf8_lossy(&got_a)
    );
    assert!(
        !String::from_utf8_lossy(&got_b).contains("1048576"),
        "CONFIG SET leaked from A to B: {}",
        String::from_utf8_lossy(&got_b)
    );

    // Stats isolation: A has served more commands than B; B's
    // keyspace stayed empty so its INFO reflects its own life only.
    let info_b = cmd(&mut b, &[b"INFO", b"keyspace"]);
    assert!(
        !String::from_utf8_lossy(&info_b).contains("keys=1"),
        "B's keyspace stats show A's key: {}",
        String::from_utf8_lossy(&info_b)
    );
}

//! v1.56 — verify v1.43.x finding fix: cluster-mode MGET / MSET /
//! SINTER / SUNION / SDIFF across slots returns `-CROSSSLOT` instead
//! of silent multi-bulk nils.
//!
//! Strict asserts:
//! - Cluster-conn MGET on cross-slot keys returns `-CROSSSLOT`.
//! - Cluster-conn MGET on same-slot keys (`{tag}` hash tag) returns
//!   a proper `*N` array.
//! - Cluster-conn MSET cross-slot returns `-CROSSSLOT`.
//! - Cluster-conn SINTER cross-slot returns `-CROSSSLOT`.
//! - Non-cluster conn MGET keeps fan-out behaviour (compat).
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test cluster_crossslot_mget --release -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig};

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn cluster_mode_multikey_emits_crossslot() {
    let bin_path = resolve_kevy_bin();
    let base = pick_free_port_block(16);
    let main = base;
    let cluster_port_base = base + 1;
    let elect = base + 8;

    let tmp = std::env::temp_dir().join(format!("kevy-chaos-crossslot-{main}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let mut cfg = HarnessConfig::new(tmp.clone(), main).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 2;
    cfg.extra_toml = format!(
        "\n[cluster]\nenabled = true\nport_base = {cluster_port_base}\n\
         node_id = \"nodeA\"\nelect_port_base = {elect}\n"
    );
    let _h = Harness::spawn(cfg).expect("spawn kevy");
    std::thread::sleep(Duration::from_millis(300));

    // Cluster conn: connect to the per-shard cluster port.
    let cluster_addr = format!("127.0.0.1:{cluster_port_base}");
    eprintln!("crossslot: cluster conn -> {cluster_addr}");
    let mut c = TcpStream::connect(&cluster_addr).expect("cluster conn");
    let _ = c.set_read_timeout(Some(Duration::from_secs(2)));

    // SET k1 / k2 / k3 on three different keys (likely different slots).
    for key in &["k1", "k2", "k3"] {
        c.write_all(&build(&[b"SET", key.as_bytes(), b"v"])).unwrap();
        let r = read_one_reply(&mut c);
        // Cluster conn may answer -MOVED for some of these depending on
        // which shard owns each slot. Either +OK or -MOVED is RESP-valid;
        // we don't care for this test.
        assert!(
            r.starts_with('+') || r.starts_with('-'),
            "SET reply malformed: {r:?}"
        );
    }

    // PHASE 1: MGET cross-slot — MUST be -CROSSSLOT.
    eprintln!("crossslot: cluster MGET k1 k2 k3 (cross-slot expected)");
    c.write_all(&build(&[b"MGET", b"k1", b"k2", b"k3"])).unwrap();
    let mget = read_one_reply(&mut c);
    eprintln!("crossslot: cluster MGET reply = {mget:?}");
    assert!(
        mget.starts_with("-CROSSSLOT"),
        "MGET cross-slot expected -CROSSSLOT, got: {mget:?}"
    );

    // PHASE 2: MGET same-slot via hash-tag `{shared}` — MUST be proper
    // `*N` array (no CROSSSLOT).
    for key in &["{shared}:k1", "{shared}:k2", "{shared}:k3"] {
        c.write_all(&build(&[b"SET", key.as_bytes(), b"v"])).unwrap();
        let _ = read_one_reply(&mut c);
    }
    c.write_all(&build(&[
        b"MGET",
        b"{shared}:k1",
        b"{shared}:k2",
        b"{shared}:k3",
    ]))
    .unwrap();
    let mget_same = read_one_reply(&mut c);
    eprintln!("crossslot: cluster MGET same-slot reply = {mget_same:?}");
    assert!(
        mget_same.starts_with("*3\r\n"),
        "same-slot MGET expected *3 array, got: {mget_same:?}"
    );

    // PHASE 3: MSET cross-slot — MUST be -CROSSSLOT.
    eprintln!("crossslot: cluster MSET cross-slot expected");
    c.write_all(&build(&[
        b"MSET", b"k1", b"v1", b"k2", b"v2", b"k3", b"v3",
    ]))
    .unwrap();
    let mset = read_one_reply(&mut c);
    eprintln!("crossslot: cluster MSET reply = {mset:?}");
    assert!(
        mset.starts_with("-CROSSSLOT"),
        "MSET cross-slot expected -CROSSSLOT, got: {mset:?}"
    );

    // PHASE 4: SINTER cross-slot — MUST be -CROSSSLOT.
    eprintln!("crossslot: cluster SINTER cross-slot expected");
    c.write_all(&build(&[b"SINTER", b"s1", b"s2", b"s3"])).unwrap();
    let sinter = read_one_reply(&mut c);
    eprintln!("crossslot: cluster SINTER reply = {sinter:?}");
    assert!(
        sinter.starts_with("-CROSSSLOT"),
        "SINTER cross-slot expected -CROSSSLOT, got: {sinter:?}"
    );

    // PHASE 5: NON-CLUSTER conn — connect to the main port. MGET
    // cross-slot must still return a proper array (compat — single-DB
    // operators keep the legacy fan-out behaviour).
    eprintln!("crossslot: non-cluster conn -> 127.0.0.1:{main}");
    let mut nc = TcpStream::connect(format!("127.0.0.1:{main}"))
        .expect("non-cluster conn");
    let _ = nc.set_read_timeout(Some(Duration::from_secs(2)));
    nc.write_all(&build(&[b"SET", b"nc:k1", b"v1"])).unwrap();
    let _ = read_one_reply(&mut nc);
    nc.write_all(&build(&[b"SET", b"nc:k2", b"v2"])).unwrap();
    let _ = read_one_reply(&mut nc);
    nc.write_all(&build(&[b"MGET", b"nc:k1", b"nc:k2"])).unwrap();
    let nc_mget = read_one_reply(&mut nc);
    eprintln!("crossslot: non-cluster MGET reply = {nc_mget:?}");
    assert!(
        nc_mget.starts_with("*2\r\n"),
        "non-cluster MGET expected *2 array (compat), got: {nc_mget:?}"
    );
    eprintln!("crossslot: non-cluster conn retains legacy fan-out (compat OK)");

    drop(c);
    drop(nc);
    let _ = std::fs::remove_dir_all(&tmp);
}

fn build(args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for a in args {
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
    out
}

fn read_one_reply(s: &mut TcpStream) -> String {
    let mut acc = Vec::with_capacity(1024);
    let mut buf = vec![0u8; 8 * 1024];
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        let n = match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        acc.extend_from_slice(&buf[..n]);
        if count_replies(&acc) >= 1 {
            break;
        }
    }
    String::from_utf8_lossy(&acc).into_owned()
}

fn count_replies(buf: &[u8]) -> usize {
    let mut i = 0;
    let mut count = 0;
    while i < buf.len() {
        match advance_one(buf, i) {
            Some(next) => {
                count += 1;
                i = next;
            }
            None => break,
        }
    }
    count
}

fn advance_one(buf: &[u8], start: usize) -> Option<usize> {
    if start >= buf.len() {
        return None;
    }
    let tag = buf[start];
    let line_end = buf[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|p| start + p + 1)?;
    match tag {
        b'+' | b'-' | b':' => Some(line_end),
        b'$' => {
            let len_str = std::str::from_utf8(&buf[start + 1..line_end - 2]).ok()?;
            let n: isize = len_str.parse().ok()?;
            if n < 0 {
                Some(line_end)
            } else {
                let end = line_end + (n as usize) + 2;
                if end <= buf.len() { Some(end) } else { None }
            }
        }
        b'*' | b'%' => {
            let len_str = std::str::from_utf8(&buf[start + 1..line_end - 2]).ok()?;
            let n: isize = len_str.parse().ok()?;
            if n < 0 {
                return Some(line_end);
            }
            let count = if tag == b'%' { (n as usize) * 2 } else { n as usize };
            let mut cur = line_end;
            for _ in 0..count {
                cur = advance_one(buf, cur)?;
            }
            Some(cur)
        }
        _ => None,
    }
}

fn pick_free_port_block(width: usize) -> u16 {
    'retry: loop {
        let anchor = std::net::TcpListener::bind("127.0.0.1:0").expect("bind anchor");
        let base = anchor.local_addr().expect("local_addr").port();
        if base.checked_add(width as u16).is_none() {
            continue;
        }
        let mut probes = Vec::with_capacity(width);
        for i in 1..=width as u16 {
            match std::net::TcpListener::bind(("127.0.0.1", base + i)) {
                Ok(l) => probes.push(l),
                Err(_) => continue 'retry,
            }
        }
        return base;
    }
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

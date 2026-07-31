//! The SEGMENTED stitch frame through the server replay path: rows the
//! frame covers come out of the hot layer on restart, later writes
//! revive, and a frame naming a segment the manifest does not hold
//! refuses startup instead of silently dropping rows.
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test segmented_stitch_e2e --release -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig, KillSignal, pick_free_port};

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

fn connect(port: u16) -> TcpStream {
    for _ in 0..50 {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
            s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            return s;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("kevy did not come up on {port}");
}

fn send(s: &mut TcpStream, req: &[u8]) -> String {
    s.write_all(req).unwrap();
    let mut buf = [0u8; 512];
    let n = s.read(&mut buf).unwrap();
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

fn cmd(args: &[&[u8]]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
    out
}

fn seal_segment(data_dir: &Path, file: &str, keys: &[&[u8]]) {
    let segs = data_dir.join("segs-0");
    std::fs::create_dir_all(&segs).unwrap();
    let mut b = kevy_seg::SegBuilder::create(&segs.join(file)).unwrap();
    let mut sorted: Vec<&[u8]> = keys.to_vec();
    sorted.sort();
    for k in &sorted {
        b.push(k, b"cold-copy").unwrap();
    }
    let meta = b.finish().unwrap();
    let mut m = kevy_seg::Manifest::open(&segs).unwrap();
    m.add(kevy_seg::ManifestEntry {
        file: file.to_string(),
        meta: Vec::new(),
        min_key: meta.min_key,
        max_key: meta.max_key,
        records: meta.records,
    })
    .unwrap();
}

fn append_frame(data_dir: &Path, args: &[&[u8]]) {
    let payload = cmd(args);
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(data_dir.join("aof-0.aof"))
        .unwrap();
    f.write_all(&(payload.len() as u32).to_le_bytes()).unwrap();
    f.write_all(&kevy_sys::checksum::crc32c(&payload).to_le_bytes()).unwrap();
    f.write_all(&payload).unwrap();
}

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn stitch_evicts_revives_and_refuses_through_the_server() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-segstitch-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("always");
    cfg.kevy_bin = bin_path;
    cfg.threads = 1;
    let mut h = Harness::spawn(cfg).expect("spawn kevy");

    let mut c = connect(port);
    for (k, v) in [(b"row:1", b"v1"), (b"row:2", b"v2"), (b"row:3", b"v3")] {
        let r = send(&mut c, &cmd(&[b"SET", k.as_slice(), v.as_slice()]));
        assert!(r.starts_with("+OK"), "SET got: {r:?}");
    }
    drop(c);
    h.kill(KillSignal::Sigkill).expect("kill");

    // The eviction's durable half, done by hand: segment + manifest,
    // then the stitch frame, then a revival write for row:1.
    seal_segment(&tmp, "b1.seg", &[b"row:1", b"row:2"]);
    append_frame(&tmp, &[kevy_persist::SEGMENTED, b"b1.seg"]);
    append_frame(&tmp, &[b"SET", b"row:1", b"revived"]);

    h.restart().expect("restart");
    let mut c = connect(port);
    assert_eq!(send(&mut c, &cmd(&[b"GET", b"row:1"])), "$7\r\nrevived\r\n", "revival lost");
    assert_eq!(send(&mut c, &cmd(&[b"GET", b"row:2"])), "$-1\r\n", "evicted row came back");
    assert_eq!(send(&mut c, &cmd(&[b"GET", b"row:3"])), "$2\r\nv3\r\n", "unsegmented row lost");
    drop(c);
    h.kill(KillSignal::Sigkill).expect("kill 2");

    // Damage the truth set: a frame naming a segment the manifest does
    // not hold. Startup must refuse — the port never comes up.
    append_frame(&tmp, &[kevy_persist::SEGMENTED, b"ghost.seg"]);
    let refused = h.restart().is_err()
        || TcpStream::connect(("127.0.0.1", port)).is_err() && {
            std::thread::sleep(Duration::from_millis(1500));
            TcpStream::connect(("127.0.0.1", port)).is_err()
        };
    assert!(refused, "server started over a damaged segment truth set");
}

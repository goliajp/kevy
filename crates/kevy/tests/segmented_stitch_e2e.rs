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

/// One row to seal: its key and its hash fields.
type RowSpec<'a> = (&'a [u8], &'a [(&'a [u8], &'a [u8])]);

/// tier-codec hash body: [nfields u32] then [flen][f][vlen][v] each.
fn hash_body(fields: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut out = (fields.len() as u32).to_le_bytes().to_vec();
    for (f, v) in fields {
        out.extend_from_slice(&(f.len() as u32).to_le_bytes());
        out.extend_from_slice(f);
        out.extend_from_slice(&(v.len() as u32).to_le_bytes());
        out.extend_from_slice(v);
    }
    out
}

fn seal_segment(data_dir: &Path, file: &str, rows: &[RowSpec<'_>]) {
    let segs = data_dir.join("segs-0");
    std::fs::create_dir_all(&segs).unwrap();
    let mut b = kevy_seg::SegBuilder::create(&segs.join(file)).unwrap();
    let mut sorted: Vec<_> = rows.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    for (k, fields) in &sorted {
        b.push(k, &hash_body(fields)).unwrap();
    }
    let meta = b.finish().unwrap();
    let mut m = kevy_seg::Manifest::open(&segs).unwrap();
    m.add(kevy_seg::ManifestEntry {
        file: file.to_string(),
        meta: b"rowcold:74".to_vec(),
        min_key: meta.min_key,
        max_key: meta.max_key,
        records: meta.records,
    })
    .unwrap();
}

fn append_frame(data_dir: &Path, args: &[&[u8]]) {
    let payload = cmd(args);
    let mut f = std::fs::OpenOptions::new().append(true).open(data_dir.join("aof-0.aof")).unwrap();
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
    for k in [b"row:1".as_slice(), b"row:2", b"row:3"] {
        let r = send(&mut c, &cmd(&[b"HSET", k, b"id", k, b"note", b"hot"]));
        assert!(r.starts_with(":"), "HSET got: {r:?}");
    }
    drop(c);
    h.kill(KillSignal::Sigkill).expect("kill");

    // The eviction's durable half, done by hand: segment + manifest,
    // then the stitch frame, then a revival write for row:1.
    seal_segment(
        &tmp,
        "row-74-0.seg",
        &[
            (b"row:1", &[(b"id", b"row:1"), (b"note", b"hot")]),
            (b"row:2", &[(b"id", b"row:2"), (b"note", b"hot")]),
        ],
    );
    append_frame(&tmp, &[kevy_persist::SEGMENTED, b"row-74-0.seg"]);
    append_frame(&tmp, &[b"HSET", b"row:1", b"note", b"revived"]);

    h.restart().expect("restart");
    let mut c = connect(port);
    let r1 = send(&mut c, &cmd(&[b"HGET", b"row:1", b"note"]));
    assert_eq!(r1, "$7\r\nrevived\r\n", "revival lost");
    // The stitched row phase-changed to a stub — and still answers,
    // served from the segment.
    let r2 = send(&mut c, &cmd(&[b"HGET", b"row:2", b"note"]));
    assert_eq!(r2, "$3\r\nhot\r\n", "stitched row unreadable");
    let r3 = send(&mut c, &cmd(&[b"HGET", b"row:3", b"note"]));
    assert_eq!(r3, "$3\r\nhot\r\n", "unsegmented row lost");
    drop(c);
    h.kill(KillSignal::Sigkill).expect("kill 2");

    // Damage the truth set: a frame naming a segment the manifest does
    // not hold. Startup must refuse — the port never comes up.
    append_frame(&tmp, &[kevy_persist::SEGMENTED, b"row-74-9.seg"]);
    let refused = h.restart().is_err()
        || TcpStream::connect(("127.0.0.1", port)).is_err() && {
            std::thread::sleep(Duration::from_millis(1500));
            TcpStream::connect(("127.0.0.1", port)).is_err()
        };
    assert!(refused, "server started over a damaged segment truth set");
}

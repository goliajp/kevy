//! Hash field TTLs — real-server e2e: the relative `HEXPIRE`
//! must survive an AOF replay WITHOUT re-anchoring (the
//! `HPEXPIREAT` follow-up frame `Shard::log_write` appends), and the
//! reaper must sweep due fields.

use std::io::{Read, Write};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

fn cmd(s: &mut std::net::TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    s.write_all(&req(parts)).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    let mut buf = [0u8; 4096];
    let n = s.read(&mut buf).unwrap();
    buf[..n].to_vec()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn boot(port: u16, dir: std::path::PathBuf, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(4)).bind([127, 0, 0, 1], port).shards(4)
            .with_data_dir(dir)
            .with_aof(true);
        rt.run(stop).unwrap();
    })
}

fn wait_up(port: u16) {
    for _ in 0..400 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("server did not come up");
}

#[test]
fn hexpire_survives_replay_without_reanchor() {
    let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = std::env::temp_dir().join(format!(
        "kevy-hfttl-e2e-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let port1 = free_port();
    let stop1 = Arc::new(AtomicBool::new(false));
    let h1 = boot(port1, dir.clone(), stop1.clone());
    wait_up(port1);
    let observed_ms: i64;
    {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", port1)).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        cmd(&mut c, &[b"HSET", b"eh", b"f", b"v", b"plain", b"p"]);
        // relative form: 100s
        let r = cmd(&mut c, &[b"HEXPIRE", b"eh", b"100", b"FIELDS", b"1", b"f"]);
        assert_eq!(r, b"*1\r\n:1\r\n");
        let r = cmd(&mut c, &[b"HTTL", b"eh", b"FIELDS", b"1", b"f"]);
        let s = String::from_utf8_lossy(&r);
        observed_ms = s.trim_start_matches("*1\r\n:").trim_end().parse().unwrap();
        assert!(observed_ms > 90_000 && observed_ms <= 100_000, "{observed_ms}");
    }
    // sleep a beat so a re-anchored replay would visibly RESET to ~100s
    std::thread::sleep(std::time::Duration::from_millis(1200));
    stop1.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port1));
    h1.join().unwrap();

    let port2 = free_port();
    let stop2 = Arc::new(AtomicBool::new(false));
    let h2 = boot(port2, dir.clone(), stop2.clone());
    wait_up(port2);
    {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", port2)).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        let r = cmd(&mut c, &[b"HTTL", b"eh", b"FIELDS", b"2", b"f", b"plain"]);
        let s = String::from_utf8_lossy(&r);
        let mut nums = s.lines().filter(|l| l.starts_with(':')).map(|l| l[1..].parse::<i64>().unwrap());
        let f_ttl = nums.next().unwrap();
        let plain = nums.next().unwrap();
        // wall clock advanced ≥1.2s across restart: a correctly
        // anchored deadline reads BELOW the pre-restart observation.
        assert!(
            f_ttl > 0 && f_ttl < observed_ms - 1000,
            "no re-anchor: pre {observed_ms}ms post {f_ttl}ms"
        );
        assert_eq!(plain, -1);
        // due field sweeps: set a past deadline and read it gone
        let r = cmd(&mut c, &[b"HPEXPIREAT", b"eh", b"10", b"FIELDS", b"1", b"f"]);
        assert_eq!(r, b"*1\r\n:2\r\n");
        let r = cmd(&mut c, &[b"HEXISTS", b"eh", b"f"]);
        assert_eq!(r, b":0\r\n");
    }
    stop2.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port2));
    h2.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

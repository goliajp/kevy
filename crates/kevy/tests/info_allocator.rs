//! `INFO allocator` — the accounting identity on a live server.
//!
//! `bench/V5-ACCOUNTING-CONTRACT.md` specifies INFO as the transport for
//! kevy-alloc's nine terms. The contract was written first and the
//! section did not exist, so the one workload where this allocator loses
//! to glibc could be measured and not attributed.
//!
//! Both halves are checked, and each runs somewhere:
//!
//! * feature ON (`cargo test -p kevy --features kevy-alloc`, wired into
//!   allocgate): the section is there and `mapped == accounted`. That
//!   sum is read off a heap nothing in this file filled, so it is a
//!   witness and not an echo of a value the test set.
//! * feature OFF (the default, so `cargo test --workspace` runs it): the
//!   section is ABSENT. An all-zero `# Allocator` under the system
//!   allocator would be a section that says something false, and INFO's
//!   bytes stay what they were before this existed — the same
//!   requirement `# Tiering` carries.

use std::io::{Read, Write};

// The feature links kevy-alloc; this attribute is what makes it the
// allocator, and it lives in `main.rs` — so a test binary that only
// turned the feature on would measure a heap serving nothing and read
// its nine zeroes as agreement. Declaring it here puts the real subject
// under the test.
#[cfg(feature = "kevy-alloc")]
#[global_allocator]
static GLOBAL: kevy_alloc::KevyAlloc = kevy_alloc::KevyAlloc;

use kevy_testnet::free_port;

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(nshards: usize) -> Server {
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-info-alloc-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (stop_thread, dir_thread) = (stop.clone(), dir.clone());
        let handle = std::thread::spawn(move || {
            kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(nshards))
                .bind([127, 0, 0, 1], port)
                .shards(nshards)
                .with_data_dir(dir_thread)
                .run(stop_thread)
                .unwrap();
        });
        for _ in 0..200 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Server { port, dir, stop, handle: Some(handle) };
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("runtime did not come up");
    }

    fn connect(&self) -> std::net::TcpStream {
        std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            drop(h.join());
        }
        drop(std::fs::remove_dir_all(&self.dir));
    }
}

fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

/// Read one RESP bulk reply and return its body.
fn read_bulk(s: &mut std::net::TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        s.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
        if head.ends_with(b"\r\n") {
            break;
        }
    }
    assert_eq!(head[0], b'$', "not a bulk reply: {:?}", String::from_utf8_lossy(&head));
    let n: usize = std::str::from_utf8(&head[1..head.len() - 2]).unwrap().parse().unwrap();
    let mut body = vec![0u8; n + 2];
    s.read_exact(&mut body).unwrap();
    body.truncate(n);
    String::from_utf8(body).unwrap()
}

/// Some traffic, so the heap under test has served the engine rather
/// than only its startup.
fn info_after_traffic(srv: &Server, section: &str) -> String {
    let mut c = srv.connect();
    for i in 0..500u32 {
        c.write_all(&req(&[b"SET", format!("k{i}").as_bytes(), &[b'v'; 200]])).unwrap();
        let mut ok = [0u8; 5];
        c.read_exact(&mut ok).unwrap();
        assert_eq!(&ok, b"+OK\r\n");
    }
    c.write_all(&req(&[b"INFO", section.as_bytes()])).unwrap();
    read_bulk(&mut c)
}

#[cfg(feature = "kevy-alloc")]
fn field(body: &str, name: &str) -> u64 {
    let prefix = format!("{name}:");
    body.lines()
        .find_map(|l| l.trim_end().strip_prefix(prefix.as_str()))
        .unwrap_or_else(|| panic!("no `{name}` in:\n{body}"))
        .parse()
        .unwrap()
}

#[cfg(feature = "kevy-alloc")]
#[test]
fn allocator_section_terms_sum_to_mapped() {
    let srv = Server::start(2);
    let body = info_after_traffic(&srv, "allocator");

    // The floor: a section that reported nothing must not read as
    // agreement between two zeroes.
    let shards = field(&body, "alloc_shards_reporting");
    assert!(shards >= 1, "no shard published a snapshot:\n{body}");
    assert!(field(&body, "alloc_mapped") > 0, "an empty heap after 500 SETs:\n{body}");
    assert!(field(&body, "alloc_live") > 0, "nothing live after 500 SETs:\n{body}");

    // The identity: every mapped byte in exactly one of the nine terms.
    // `accounted` is printed rather than derived here for this reason —
    // the two numbers come from different code and are compared, so a
    // term dropped from the section shows up as a gap.
    assert_eq!(
        field(&body, "alloc_mapped"),
        field(&body, "alloc_accounted"),
        "the terms do not sum to mapped:\n{body}"
    );

    // Every named term is present. A section that quietly loses one
    // still balances (the gap moves into another bucket) — so their
    // presence is checked, not just their sum.
    for term in [
        "alloc_live",
        "alloc_rounding",
        "alloc_cache",
        "alloc_span_free",
        "alloc_returned",
        "alloc_virgin",
        "alloc_hysteresis",
        "alloc_segment_overhead",
    ] {
        assert!(body.contains(&format!("{term}:")), "missing {term}:\n{body}");
    }
}

#[cfg(not(feature = "kevy-alloc"))]
#[test]
fn allocator_section_is_absent_on_the_system_allocator() {
    let srv = Server::start(2);
    // `INFO` with no argument: the default set, which is where a section
    // that should not exist would show up for every existing reader.
    let body = info_after_traffic(&srv, "default");
    assert!(!body.contains("# Allocator"), "system allocator emitted a section:\n{body}");
    assert!(!body.contains("alloc_mapped"), "system allocator emitted terms:\n{body}");
    // The check has a floor of its own: an INFO that returned nothing
    // would pass both assertions above without meaning anything.
    assert!(body.contains("# Memory"), "INFO default came back without its sections:\n{body}");
}

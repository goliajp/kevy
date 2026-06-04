//! `GEOADD` / `GEOPOS` / `GEODIST` / `GEOHASH` — basic Redis GEO
//! quartet (v2-6 sprint A). End-to-end via a real TCP server so the
//! dispatch + write-classification + ZSet backing all stay wired.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
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

fn read_n(s: &mut std::net::TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).unwrap();
    buf
}

/// Read a full RESP reply by parsing the first byte and chasing length
/// prefixes. Enough for the GEO replies we care about (Int, Bulk, Array,
/// Error, Simple string). Not a general parser — keeps the test file
/// dependency-free.
fn read_reply(s: &mut std::net::TcpStream) -> Vec<u8> {
    let head = read_n(s, 1);
    let mut out = head.clone();
    match head[0] {
        b'+' | b'-' | b':' => read_line(s, &mut out),
        b'$' => {
            let len = read_len_line(s, &mut out);
            if len < 0 {
                return out;
            }
            out.extend_from_slice(&read_n(s, len as usize + 2));
        }
        b'*' => {
            let n = read_len_line(s, &mut out);
            if n < 0 {
                return out;
            }
            for _ in 0..n {
                out.extend_from_slice(&read_reply(s));
            }
        }
        other => panic!("unknown reply prefix {other:?}: {:?}", out),
    }
    out
}

fn read_line(s: &mut std::net::TcpStream, out: &mut Vec<u8>) {
    loop {
        let b = read_n(s, 1);
        out.extend_from_slice(&b);
        if out.ends_with(b"\r\n") {
            break;
        }
    }
}

fn read_len_line(s: &mut std::net::TcpStream, out: &mut Vec<u8>) -> i64 {
    let start = out.len();
    read_line(s, out);
    let line = &out[start..out.len() - 2];
    std::str::from_utf8(line).unwrap().parse().unwrap()
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(nshards: usize) -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(|e| e.into_inner());
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-geo-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, nshards, kevy::KevyCommands)
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        for _ in 0..200 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Self { port, dir, stop, handle: Some(handle) }
    }

    fn connect(&self) -> std::net::TcpStream {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        s
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn add_sicily(c: &mut std::net::TcpStream) {
    c.write_all(&req(&[
        b"GEOADD",
        b"Sicily",
        b"13.361389",
        b"38.115556",
        b"Palermo",
        b"15.087269",
        b"37.502669",
        b"Catania",
    ]))
    .unwrap();
    assert_eq!(read_reply(c), b":2\r\n");
}

#[test]
fn geoadd_returns_count_of_new_members() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    // Re-adding the same members → 0 new.
    c.write_all(&req(&[
        b"GEOADD",
        b"Sicily",
        b"13.361389",
        b"38.115556",
        b"Palermo",
    ]))
    .unwrap();
    assert_eq!(read_reply(&mut c), b":0\r\n");
}

#[test]
fn geoadd_rejects_out_of_range_coordinates() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"GEOADD", b"k", b"0", b"86", b"m"])).unwrap();
    let r = read_reply(&mut c);
    assert!(
        r.starts_with(b"-ERR invalid longitude,latitude"),
        "got: {:?}",
        String::from_utf8_lossy(&r),
    );
}

#[test]
fn geoadd_nx_only_inserts_when_missing() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    // NX: Palermo exists, so this should be a no-op.
    c.write_all(&req(&[
        b"GEOADD",
        b"Sicily",
        b"NX",
        b"99.0",
        b"0.0",
        b"Palermo",
    ]))
    .unwrap();
    assert_eq!(read_reply(&mut c), b":0\r\n");
    // Confirm Palermo's coords are unchanged.
    c.write_all(&req(&[b"GEOPOS", b"Sicily", b"Palermo"])).unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("13.361389"), "Palermo coords mutated: {s}");
}

#[test]
fn geoadd_xx_only_updates_when_present() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    c.write_all(&req(&[
        b"GEOADD",
        b"Sicily",
        b"XX",
        b"0.0",
        b"0.0",
        b"Newcomer",
    ]))
    .unwrap();
    assert_eq!(read_reply(&mut c), b":0\r\n");
    c.write_all(&req(&[b"GEOPOS", b"Sicily", b"Newcomer"])).unwrap();
    assert_eq!(read_reply(&mut c), b"*1\r\n*-1\r\n");
}

#[test]
fn geopos_returns_coordinates_for_known_members() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    c.write_all(&req(&[b"GEOPOS", b"Sicily", b"Palermo", b"Nope"]))
        .unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("*2\r\n"), "expected 2-element array, got: {s}");
    assert!(s.contains("13.361389"), "Palermo lon missing: {s}");
    assert!(s.contains("38.115556"), "Palermo lat missing: {s}");
    assert!(s.contains("*-1\r\n"), "missing nil for missing member: {s}");
}

#[test]
fn geodist_palermo_catania_kilometres() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    c.write_all(&req(&[b"GEODIST", b"Sicily", b"Palermo", b"Catania", b"km"]))
        .unwrap();
    let r = read_reply(&mut c);
    // Distance ≈ 166.27 km. Reply is a bulk string with 4 decimals.
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("166."), "expected ~166 km, got: {s}");
}

#[test]
fn geodist_missing_member_returns_nil() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    c.write_all(&req(&[b"GEODIST", b"Sicily", b"Palermo", b"Nope"]))
        .unwrap();
    assert_eq!(read_reply(&mut c), b"$-1\r\n");
}

#[test]
fn geohash_emits_11_char_base32() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    c.write_all(&req(&[b"GEOHASH", b"Sicily", b"Palermo", b"Catania"]))
        .unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    // First 10 chars must match Redis exactly; the 11th carries 2 bits
    // sensitive to IEEE-754 precision — see kevy-geo for the rationale.
    assert!(s.contains("sqc8b49rny"), "Palermo geohash drift: {s}");
    assert!(s.contains("sqdtr74hyu"), "Catania geohash drift: {s}");
}

// ───────────── GEOSEARCH ─────────────

fn add_two_more(c: &mut std::net::TcpStream) {
    // Agrigento (south coast, ~120 km from Palermo)
    // Roma (mainland, ~430 km from Palermo)
    c.write_all(&req(&[
        b"GEOADD",
        b"Sicily",
        b"13.583333",
        b"37.318333",
        b"Agrigento",
        b"12.496366",
        b"41.902782",
        b"Roma",
    ]))
    .unwrap();
    assert_eq!(read_reply(c), b":2\r\n");
}

#[test]
fn geosearch_byradius_fromlonlat_returns_members_within_radius() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    add_two_more(&mut c);
    c.write_all(&req(&[
        b"GEOSEARCH",
        b"Sicily",
        b"FROMLONLAT",
        b"13.361389",
        b"38.115556",
        b"BYRADIUS",
        b"200",
        b"km",
    ]))
    .unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    // 200 km around Palermo covers Palermo, Catania (166 km), Agrigento
    // (~120 km), but NOT Roma (~430 km).
    assert!(s.contains("Palermo"), "Palermo missing: {s}");
    assert!(s.contains("Catania"), "Catania missing: {s}");
    assert!(s.contains("Agrigento"), "Agrigento missing: {s}");
    assert!(!s.contains("Roma"), "Roma should be out of range: {s}");
}

#[test]
fn geosearch_byradius_frommember_with_self_match() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    add_two_more(&mut c);
    c.write_all(&req(&[
        b"GEOSEARCH",
        b"Sicily",
        b"FROMMEMBER",
        b"Palermo",
        b"BYRADIUS",
        b"50",
        b"km",
    ]))
    .unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    // Only Palermo within 50 km of itself.
    assert!(s.contains("Palermo"), "Palermo missing: {s}");
    assert!(!s.contains("Catania"), "Catania too far for 50km: {s}");
}

#[test]
fn geosearch_frommember_unknown_member_errors() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    c.write_all(&req(&[
        b"GEOSEARCH",
        b"Sicily",
        b"FROMMEMBER",
        b"NoSuchMember",
        b"BYRADIUS",
        b"50",
        b"km",
    ]))
    .unwrap();
    let r = read_reply(&mut c);
    assert!(
        r.starts_with(b"-ERR could not decode requested zset member"),
        "got: {:?}",
        String::from_utf8_lossy(&r),
    );
}

#[test]
fn geosearch_asc_orders_by_distance() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    add_two_more(&mut c);
    c.write_all(&req(&[
        b"GEOSEARCH",
        b"Sicily",
        b"FROMLONLAT",
        b"13.361389",
        b"38.115556",
        b"BYRADIUS",
        b"500",
        b"km",
        b"ASC",
    ]))
    .unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    // Order: Palermo (0), Agrigento (~120), Catania (166), Roma (~430).
    let p = s.find("Palermo").unwrap();
    let a = s.find("Agrigento").unwrap();
    let c_i = s.find("Catania").unwrap();
    let r_i = s.find("Roma").unwrap();
    assert!(
        p < a && a < c_i && c_i < r_i,
        "ASC order broken: {s}",
    );
}

#[test]
fn geosearch_count_truncates_results() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    add_two_more(&mut c);
    c.write_all(&req(&[
        b"GEOSEARCH",
        b"Sicily",
        b"FROMLONLAT",
        b"13.361389",
        b"38.115556",
        b"BYRADIUS",
        b"500",
        b"km",
        b"COUNT",
        b"2",
    ]))
    .unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    // Two closest: Palermo + Agrigento.
    assert!(s.starts_with("*2\r\n"), "expected 2 members: {s}");
    assert!(s.contains("Palermo"));
    assert!(s.contains("Agrigento"));
    assert!(!s.contains("Catania"), "COUNT 2 should drop Catania: {s}");
}

#[test]
fn geosearch_withcoord_withdist_withhash_emit_nested_arrays() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    c.write_all(&req(&[
        b"GEOSEARCH",
        b"Sicily",
        b"FROMLONLAT",
        b"13.361389",
        b"38.115556",
        b"BYRADIUS",
        b"50",
        b"km",
        b"WITHCOORD",
        b"WITHDIST",
        b"WITHHASH",
    ]))
    .unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    // Self-match returns an inner *4 array: name + dist + hash + [lon, lat].
    assert!(s.contains("*1\r\n*4\r\n"), "expected nested array: {s}");
    assert!(s.contains("Palermo"));
    assert!(s.contains("13.361389"), "WITHCOORD lon missing: {s}");
}

#[test]
fn geosearch_bybox_filters_to_rectangle() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    add_two_more(&mut c);
    // Box around Palermo: 400 km wide, 100 km tall → captures the
    // Sicilian east-west axis (Catania ~120 km east-southeast within
    // the box width but the box height is only 100 km so the south
    // members on the same latitude band as Palermo qualify; Catania
    // is ~70 km south of Palermo so within the 100 km tall box;
    // Agrigento is ~90 km south — also in. Roma is ~430 km north —
    // out (height 100 km too short).
    c.write_all(&req(&[
        b"GEOSEARCH",
        b"Sicily",
        b"FROMLONLAT",
        b"13.361389",
        b"38.115556",
        b"BYBOX",
        b"400",
        b"200",
        b"km",
    ]))
    .unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("Palermo"));
    assert!(s.contains("Catania"));
    assert!(s.contains("Agrigento"));
    assert!(!s.contains("Roma"), "Roma should be out of box: {s}");
}

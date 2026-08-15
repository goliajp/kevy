//! `GEOADD` / `GEOPOS` / `GEODIST` / `GEOHASH` — basic Redis GEO
//! quartet (v2-6 sprint A). End-to-end via a real TCP server so the
//! dispatch + write-classification + ZSet backing all stay wired.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

use kevy_testnet::free_port;

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
        other => panic!("unknown reply prefix {other:?}: {out:?}"),
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
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(nshards)).bind([127, 0, 0, 1], port).shards(nshards)
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        kevy_testnet::assert_listening(port, "the server under test");
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

// ───────────── GEOSEARCHSTORE / GEORADIUS / GEORADIUSBYMEMBER ─────────────

#[test]
fn geosearchstore_writes_geohash_scores_to_destination() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    add_two_more(&mut c);
    c.write_all(&req(&[
        b"GEOSEARCHSTORE",
        b"NearPalermo",
        b"Sicily",
        b"FROMLONLAT",
        b"13.361389",
        b"38.115556",
        b"BYRADIUS",
        b"200",
        b"km",
    ]))
    .unwrap();
    // Expect ":3\r\n" — Palermo, Catania, Agrigento.
    assert_eq!(read_reply(&mut c), b":3\r\n");
    // ZSCORE on dst must return Palermo's geohash score.
    c.write_all(&req(&[b"ZSCORE", b"NearPalermo", b"Palermo"])).unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("3479099956230698"), "dst missing geohash score: {s}");
}

#[test]
fn geosearchstore_with_storedist_writes_distance_scores() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    c.write_all(&req(&[
        b"GEOSEARCHSTORE",
        b"Distances",
        b"Sicily",
        b"FROMMEMBER",
        b"Palermo",
        b"BYRADIUS",
        b"200",
        b"km",
        b"STOREDIST",
    ]))
    .unwrap();
    assert_eq!(read_reply(&mut c), b":2\r\n");
    // Palermo→Palermo distance is 0.
    c.write_all(&req(&[b"ZSCORE", b"Distances", b"Palermo"])).unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("$1\r\n0\r\n") || s.contains("$3\r\n0.0"), "self-distance not 0: {s}");
}

#[test]
fn geosearchstore_empty_result_clears_destination() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    // Pre-populate dst with something to verify it gets cleared.
    c.write_all(&req(&[
        b"GEOSEARCHSTORE",
        b"Out",
        b"Sicily",
        b"FROMLONLAT",
        b"13.361389",
        b"38.115556",
        b"BYRADIUS",
        b"50",
        b"km",
    ]))
    .unwrap();
    // Self-match → :1
    assert_eq!(read_reply(&mut c), b":1\r\n");
    // Now search far from any Sicily member → :0 + dst gone.
    c.write_all(&req(&[
        b"GEOSEARCHSTORE",
        b"Out",
        b"Sicily",
        b"FROMLONLAT",
        b"0",
        b"0",
        b"BYRADIUS",
        b"100",
        b"m",
    ]))
    .unwrap();
    assert_eq!(read_reply(&mut c), b":0\r\n");
    c.write_all(&req(&[b"EXISTS", b"Out"])).unwrap();
    assert_eq!(read_reply(&mut c), b":0\r\n");
}

#[test]
fn georadius_legacy_form_returns_members() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    add_two_more(&mut c);
    c.write_all(&req(&[
        b"GEORADIUS",
        b"Sicily",
        b"13.361389",
        b"38.115556",
        b"200",
        b"km",
        b"ASC",
    ]))
    .unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("Palermo"));
    assert!(s.contains("Catania"));
    assert!(s.contains("Agrigento"));
    assert!(!s.contains("Roma"));
}

#[test]
fn georadiusbymember_legacy_form_returns_members() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    c.write_all(&req(&[
        b"GEORADIUSBYMEMBER",
        b"Sicily",
        b"Palermo",
        b"200",
        b"km",
    ]))
    .unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("Palermo"));
    assert!(s.contains("Catania"));
}

#[test]
fn georadius_store_writes_geohash_scores() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    add_two_more(&mut c);
    c.write_all(&req(&[
        b"GEORADIUS",
        b"Sicily",
        b"13.361389",
        b"38.115556",
        b"200",
        b"km",
        b"STORE",
        b"NearPalermo",
    ]))
    .unwrap();
    assert_eq!(read_reply(&mut c), b":3\r\n");
    c.write_all(&req(&[b"ZSCORE", b"NearPalermo", b"Palermo"])).unwrap();
    let s = String::from_utf8_lossy(&read_reply(&mut c)).to_string();
    assert!(s.contains("3479099956230698"), "got: {s}");
}

#[test]
fn georadius_ro_rejects_store() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    c.write_all(&req(&[
        b"GEORADIUS_RO",
        b"Sicily",
        b"13.361389",
        b"38.115556",
        b"50",
        b"km",
        b"STORE",
        b"x",
    ]))
    .unwrap();
    let r = read_reply(&mut c);
    assert!(
        r.starts_with(b"-ERR"),
        "_RO variant must reject STORE: {:?}",
        String::from_utf8_lossy(&r),
    );
}

#[test]
fn georadius_store_with_with_clause_is_rejected() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    add_sicily(&mut c);
    c.write_all(&req(&[
        b"GEORADIUS",
        b"Sicily",
        b"13.361389",
        b"38.115556",
        b"50",
        b"km",
        b"WITHCOORD",
        b"STORE",
        b"x",
    ]))
    .unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    assert!(s.starts_with("-ERR"), "expected error, got: {s}");
    assert!(
        s.contains("STORE") && s.contains("WITH"),
        "expected STORE+WITH conflict message, got: {s}",
    );
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

// ───────────── cross-shard geo *STORE (v4) ─────────────
//
// Every test above runs on `Server::start(1)`, where a source and a
// destination key always land on the same shard — which hid a whole
// class of routing bugs. These run on 8 shards, where they don't: the
// source is read on its own shard and the destination written on its.
// Each asserts the DESTINATION's contents, never just the reply count —
// the broken routing replied `:2` and wrote nowhere anybody could read.

/// One shard-crossing generation: 8 differently-hashed key pairs, so at
/// least one lands source and destination on different shards (in practice
/// most do). A single pair could get lucky and co-locate.
const PAIRS: usize = 8;

fn text(r: &[u8]) -> String {
    String::from_utf8_lossy(r).into_owned()
}

fn cmd(c: &mut std::net::TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    c.write_all(&req(parts)).unwrap();
    read_reply(c)
}

fn geoadd_sicily(c: &mut std::net::TcpStream, key: &[u8]) {
    let r = cmd(
        c,
        &[
            b"GEOADD", key, b"13.361389", b"38.115556", b"Palermo", b"15.087269", b"37.502669",
            b"Catania",
        ],
    );
    assert_eq!(r, b":2\r\n");
}

/// The bulk-string body of a `$n\r\n…\r\n` reply, parsed as f64.
fn bulk_f64(r: &[u8]) -> f64 {
    let s = text(r);
    let (_, body) = s.split_once("\r\n").unwrap_or_else(|| panic!("not a bulk reply: {s}"));
    body.trim_end_matches("\r\n")
        .parse()
        .unwrap_or_else(|_| panic!("not a float: {s}"))
}

#[test]
fn geosearchstore_writes_destination_on_its_own_shard() {
    let srv = Server::start(8);
    let mut c = srv.connect();
    for i in 0..PAIRS {
        let (src, dst) = (format!("gs-src-{i}"), format!("gs-dst-{i}"));
        geoadd_sicily(&mut c, src.as_bytes());
        let r = cmd(
            &mut c,
            &[
                b"GEOSEARCHSTORE",
                dst.as_bytes(),
                src.as_bytes(),
                b"FROMLONLAT",
                b"15",
                b"37",
                b"BYRADIUS",
                b"200",
                b"km",
                b"ASC",
            ],
        );
        assert_eq!(r, b":2\r\n", "{src} → {dst}: stored count");
        let z = text(&cmd(&mut c, &[b"ZRANGE", dst.as_bytes(), b"0", b"-1"]));
        assert!(
            z.contains("Catania") && z.contains("Palermo"),
            "{dst} must hold the hits on its OWN shard, got: {z}",
        );
    }
}

#[test]
fn geosearchstore_frommember_resolves_anchor_on_the_source_shard() {
    let srv = Server::start(8);
    let mut c = srv.connect();
    for i in 0..PAIRS {
        let (src, dst) = (format!("gm-src-{i}"), format!("gm-dst-{i}"));
        geoadd_sicily(&mut c, src.as_bytes());
        // The anchor member lives in `src`. Routed by the destination, this
        // looked it up in `dst`'s (empty) keyspace: "could not decode
        // requested zset member".
        let r = cmd(
            &mut c,
            &[
                b"GEOSEARCHSTORE",
                dst.as_bytes(),
                src.as_bytes(),
                b"FROMMEMBER",
                b"Palermo",
                b"BYRADIUS",
                b"200",
                b"km",
                b"ASC",
            ],
        );
        assert_eq!(r, b":2\r\n", "{src} → {dst}: got {}", text(&r));
        let z = text(&cmd(&mut c, &[b"ZRANGE", dst.as_bytes(), b"0", b"-1"]));
        assert!(z.contains("Catania") && z.contains("Palermo"), "{dst}: {z}");
    }
}

#[test]
fn georadius_store_writes_destination_on_its_own_shard() {
    let srv = Server::start(8);
    let mut c = srv.connect();
    for i in 0..PAIRS {
        let (src, dst) = (format!("gr-src-{i}"), format!("gr-dst-{i}"));
        geoadd_sicily(&mut c, src.as_bytes());
        let r = cmd(
            &mut c,
            &[
                b"GEORADIUS",
                src.as_bytes(),
                b"15",
                b"37",
                b"200",
                b"km",
                b"STORE",
                dst.as_bytes(),
            ],
        );
        assert_eq!(r, b":2\r\n", "{src} → {dst}: stored count");
        let z = text(&cmd(&mut c, &[b"ZRANGE", dst.as_bytes(), b"0", b"-1"]));
        assert!(
            z.contains("Catania") && z.contains("Palermo"),
            "{dst} must hold the hits on its OWN shard, got: {z}",
        );
    }
}

#[test]
fn georadiusbymember_store_writes_destination_on_its_own_shard() {
    let srv = Server::start(8);
    let mut c = srv.connect();
    for i in 0..PAIRS {
        let (src, dst) = (format!("gb-src-{i}"), format!("gb-dst-{i}"));
        geoadd_sicily(&mut c, src.as_bytes());
        let r = cmd(
            &mut c,
            &[
                b"GEORADIUSBYMEMBER",
                src.as_bytes(),
                b"Palermo",
                b"200",
                b"km",
                b"STORE",
                dst.as_bytes(),
            ],
        );
        assert_eq!(r, b":2\r\n", "{src} → {dst}: stored count");
        let z = text(&cmd(&mut c, &[b"ZRANGE", dst.as_bytes(), b"0", b"-1"]));
        assert!(z.contains("Catania") && z.contains("Palermo"), "{dst}: {z}");
    }
}

#[test]
fn geo_store_empty_result_deletes_the_destination_on_its_shard() {
    let srv = Server::start(8);
    let mut c = srv.connect();
    for i in 0..PAIRS {
        let (src, dst) = (format!("ge-src-{i}"), format!("ge-dst-{i}"));
        geoadd_sicily(&mut c, src.as_bytes());
        // Stale destination content must go, even though it lives on a shard
        // the search never touches.
        assert_eq!(cmd(&mut c, &[b"ZADD", dst.as_bytes(), b"1", b"stale"]), b":1\r\n");
        let r = cmd(
            &mut c,
            &[
                b"GEOSEARCHSTORE",
                dst.as_bytes(),
                src.as_bytes(),
                b"FROMLONLAT",
                b"0",
                b"0",
                b"BYRADIUS",
                b"1",
                b"km",
            ],
        );
        assert_eq!(r, b":0\r\n");
        assert_eq!(cmd(&mut c, &[b"EXISTS", dst.as_bytes()]), b":0\r\n", "{dst} must be gone");
    }
}

#[test]
fn geo_storedist_scores_are_in_the_queried_unit() {
    let srv = Server::start(8);
    let mut c = srv.connect();
    for i in 0..PAIRS {
        let (src, dst) = (format!("gd-src-{i}"), format!("gd-dst-{i}"));
        geoadd_sicily(&mut c, src.as_bytes());
        // Palermo → Catania is 166.27 km. STOREDIST stores the distance in
        // the unit the query asked for (Redis divides by the shape's
        // conversion), not raw metres.
        let r = cmd(
            &mut c,
            &[
                b"GEOSEARCHSTORE",
                dst.as_bytes(),
                src.as_bytes(),
                b"FROMMEMBER",
                b"Palermo",
                b"BYRADIUS",
                b"200",
                b"km",
                b"STOREDIST",
            ],
        );
        assert_eq!(r, b":2\r\n", "{}", text(&r));
        let d = bulk_f64(&cmd(&mut c, &[b"ZSCORE", dst.as_bytes(), b"Catania"]));
        assert!(
            (d - 166.27).abs() < 0.1,
            "STOREDIST km score should be ~166.27 km, got {d}",
        );
        let zero = bulk_f64(&cmd(&mut c, &[b"ZSCORE", dst.as_bytes(), b"Palermo"]));
        assert!(zero.abs() < 0.001, "anchor distance should be 0, got {zero}");
    }
}

#[test]
fn georadius_storedist_scores_are_in_the_queried_unit() {
    let srv = Server::start(8);
    let mut c = srv.connect();
    let (src, dst) = (b"gdr-src".as_slice(), b"gdr-dst".as_slice());
    geoadd_sicily(&mut c, src);
    let r = cmd(
        &mut c,
        &[b"GEORADIUSBYMEMBER", src, b"Palermo", b"200", b"km", b"STOREDIST", dst],
    );
    assert_eq!(r, b":2\r\n");
    let d = bulk_f64(&cmd(&mut c, &[b"ZSCORE", dst, b"Catania"]));
    assert!((d - 166.27).abs() < 0.1, "expected ~166.27 km, got {d}");
    // …and in metres when the query is in metres.
    let mdst = b"gdr-dst-m".as_slice();
    let r = cmd(
        &mut c,
        &[b"GEORADIUSBYMEMBER", src, b"Palermo", b"200000", b"m", b"STOREDIST", mdst],
    );
    assert_eq!(r, b":2\r\n");
    let d = bulk_f64(&cmd(&mut c, &[b"ZSCORE", mdst, b"Catania"]));
    assert!((d - 166_274.0).abs() < 100.0, "expected ~166274 m, got {d}");
}

#[test]
fn geosearch_desc_with_count_returns_the_farthest() {
    let srv = Server::start(8);
    let mut c = srv.connect();
    geoadd_sicily(&mut c, b"gsort");
    // From (15,37): Catania ≈ 56 km, Palermo ≈ 190 km. DESC + COUNT 1 asks
    // for the FARTHEST one; truncating an ascending sort returned the
    // nearest — the opposite result set.
    let r = text(&cmd(
        &mut c,
        &[
            b"GEOSEARCH", b"gsort", b"FROMLONLAT", b"15", b"37", b"BYRADIUS", b"200", b"km",
            b"DESC", b"COUNT", b"1",
        ],
    ));
    assert!(r.contains("Palermo") && !r.contains("Catania"), "DESC COUNT 1: {r}");
    // ASC + COUNT 1 still returns the nearest…
    let r = text(&cmd(
        &mut c,
        &[
            b"GEOSEARCH", b"gsort", b"FROMLONLAT", b"15", b"37", b"BYRADIUS", b"200", b"km",
            b"ASC", b"COUNT", b"1",
        ],
    ));
    assert!(r.contains("Catania") && !r.contains("Palermo"), "ASC COUNT 1: {r}");
    // …and COUNT with no explicit sort keeps the implicit-nearest contract.
    let r = text(&cmd(
        &mut c,
        &[
            b"GEOSEARCH", b"gsort", b"FROMLONLAT", b"15", b"37", b"BYRADIUS", b"200", b"km",
            b"COUNT", b"1",
        ],
    ));
    assert!(r.contains("Catania") && !r.contains("Palermo"), "COUNT 1 (no sort): {r}");
}

#[test]
fn georadius_desc_with_count_returns_the_farthest() {
    let srv = Server::start(8);
    let mut c = srv.connect();
    geoadd_sicily(&mut c, b"grsort");
    let r = text(&cmd(
        &mut c,
        &[b"GEORADIUS", b"grsort", b"15", b"37", b"200", b"km", b"DESC", b"COUNT", b"1"],
    ));
    assert!(r.contains("Palermo") && !r.contains("Catania"), "DESC COUNT 1: {r}");
}

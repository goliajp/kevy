//! Export → import round-trip against two real servers.

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
        if !bin.exists() {
            // cargo doesn't know this test depends on the kevy bin;
            // under full-workspace parallelism the build order races.
            // Build it deterministically (no-op when fresh).
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = Command::new(cargo)
                .args(["build", "-p", "kevy", "--bin", "kevy"])
                .status()
                .expect("spawn cargo build");
            assert!(status.success(), "cargo build -p kevy --bin kevy failed");
        }
        assert!(bin.exists(), "kevy server binary still missing at {bin:?}");
        let child = Command::new(&bin)
            .args(["--port", &port.to_string(), "--threads", "2", "--no-aof"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn kevy server");
        // 10 s, not 2: under full-workspace parallelism this test shares the
        // machine with every other build and suite, and a server that needs
        // 3 s to come up is slow, not absent. A wait budget tuned on an idle
        // machine is a flake scheduled for the first busy one.
        for _ in 0..500 {
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
    let n = run_export(&mut cs, Some(b"mig:"), &file).unwrap().keys;
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
    let n2 = run_export(&mut cs, Some(b"mig:"), &file2).unwrap().keys;
    assert_eq!(n2, n);
    std::fs::write(file2.with_extension("progress"), b"0").unwrap();
    let rep2 = run_import(&mut cd, &file2, true, true).unwrap();
    assert_eq!(rep2.errors, 0, "idempotent replay");
    assert_eq!(digest(&mut cd, "mig:"), ds);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bulk_ops_and_diff() {
    let srv = Srv::start();
    let mut c = srv.client();
    for i in 0..200 {
        c.request_borrowed(&[b"SET", format!("bk:{i}").as_bytes(), format!("v{i}").as_bytes()]).unwrap();
    }
    c.request_borrowed(&[b"SET", b"bk:ttl", b"x"]).unwrap();
    c.request_borrowed(&[b"PEXPIRE", b"bk:ttl", b"60000"]).unwrap();

    // copy-prefix carries values + TTL (COPY REPLACE)
    let n = kevy_cli::bulk::run_copy_prefix(&mut c, b"bk:", b"ck:", 0).unwrap();
    assert_eq!(n, 201);
    let (na, da) = kevy_cli::bulk::run_digest(&mut c, b"bk:").unwrap();
    let (nb, _db) = kevy_cli::bulk::run_digest(&mut c, b"ck:").unwrap();
    assert_eq!((na, nb), (201, 201));
    // digests differ because the KEY participates in the row digest —
    // value equality is checked via a spot key + TTL carry
    let r = c.request_borrowed(&[b"GET", b"ck:42"]).unwrap();
    assert_eq!(format!("{r:?}"), format!("{:?}", kevy_cli::Reply::Bulk(b"v42".to_vec())));
    let r = c.request_borrowed(&[b"PTTL", b"ck:ttl"]).unwrap();
    let ms: i64 = format!("{r:?}").trim_start_matches("Int(").trim_end_matches(')').parse().unwrap();
    assert!(ms > 0, "COPY carries TTL: {ms}");
    let _ = da;

    // dry-run counts without deleting
    let n = kevy_cli::bulk::run_delete_prefix(&mut c, b"ck:", 0, true).unwrap();
    assert_eq!(n, 201);
    let (still, _) = kevy_cli::bulk::run_digest(&mut c, b"ck:").unwrap();
    assert_eq!(still, 201);
    // rate-limited real delete: 201 keys at 400/s ≈ 0.5s (±20% gate lives in onrampgate)
    let t0 = std::time::Instant::now();
    let n = kevy_cli::bulk::run_delete_prefix(&mut c, b"ck:", 400, false).unwrap();
    let dt = t0.elapsed().as_secs_f64();
    assert_eq!(n, 201);
    assert!(dt > 0.3, "rate limit engaged: {dt:.2}s");
    let (gone, _) = kevy_cli::bulk::run_digest(&mut c, b"ck:").unwrap();
    assert_eq!(gone, 0);

    // diff: same server twice → OK; after a mutation → MISMATCH
    let srv2 = Srv::start();
    let mut c2 = srv2.client();
    let mut c1b = srv.client();
    let mut out = Vec::new();
    let bad = kevy_cli::bulk::run_diff(&mut c1b, &mut c2, &[b"bk:".to_vec()], &mut out).unwrap();
    assert_eq!(bad.len(), 1, "empty dst mismatches: {}", String::from_utf8_lossy(&out));
    // import bk: into srv2 → diff clean
    let dir = std::env::temp_dir().join(format!("kevy-bulk-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("bk.resp");
    let mut c1c = srv.client();
    kevy_cli::migrate::run_export(&mut c1c, Some(b"bk:"), &file).unwrap();
    kevy_cli::migrate::run_import(&mut c2, &file, false, true).unwrap();
    let mut out = Vec::new();
    let bad = kevy_cli::bulk::run_diff(&mut c1c, &mut c2, &[b"bk:".to_vec()], &mut out).unwrap();
    assert!(bad.is_empty(), "{}", String::from_utf8_lossy(&out));

    // inspect renders counts
    let mut out = Vec::new();
    kevy_cli::bulk::run_inspect(&mut c1c, b"bk:", &mut out).unwrap();
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("201 keys") && s.contains("string: 201"), "{s}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every type the engine can hold must leave an export either **in the
/// file** or **in the skipped report**. Neither is optional: a type
/// that is in neither vanishes on migration day while the tool prints
/// a success line.
///
/// Streams are the type that was in neither. The export walked past
/// them, counted them as "vanished between SCAN and read", and said
/// nothing — measured on a 4007-key store that exported 4006.
#[test]
fn every_type_is_either_exported_or_reported() {
    let src = Srv::start();
    let mut cs = src.client();
    // One key per type the server will accept.
    cs.request_borrowed(&[b"SET", b"t:string", b"v"]).unwrap();
    cs.request_borrowed(&[b"RPUSH", b"t:list", b"a"]).unwrap();
    cs.request_borrowed(&[b"SADD", b"t:set", b"m"]).unwrap();
    cs.request_borrowed(&[b"HSET", b"t:hash", b"f", b"v"]).unwrap();
    cs.request_borrowed(&[b"ZADD", b"t:zset", b"1", b"m"]).unwrap();
    cs.request_borrowed(&[b"XADD", b"t:stream", b"*", b"f", b"v"]).unwrap();

    let dir = std::env::temp_dir().join(format!("kevy-types-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("types.resp");
    let out = run_export(&mut cs, Some(b"t:"), &file).unwrap();

    let accounted = out.keys + out.skipped.values().sum::<u64>();
    assert_eq!(
        accounted, 6,
        "6 keys written, {} exported + {} skipped — the difference is silent loss",
        out.keys,
        out.skipped.values().sum::<u64>()
    );
    assert_eq!(
        out.skipped.get(b"stream".as_slice()).copied(),
        Some(1),
        "the stream must be named in the report, not merely absent: {:?}",
        out.skipped.keys().map(|k| String::from_utf8_lossy(k).into_owned()).collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

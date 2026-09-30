//! repack_load — the load client bench/repack-tail.sh drives.
//!
//! Hashes `r:<i>` with an i64 field `v` under a range index `rt`, and a
//! paced mixed load against them. Every request is timed from the moment
//! it was due to be sent, not from when it went out, so a stalled server
//! is charged for the requests queued behind the stall as well.
//!
//!   repack_load ping   port=P
//!   repack_load fill   port=P rows=N [conns=4] [pad=32] [seed=1]
//!   repack_load verify port=P
//!   repack_load load   port=P rows=N secs=S rate=R conns=C [mix=30,60,10]
//!                      [limit=10] [slow_us=300] [spin=1] [seed=1] out=FILE
//!
//! `fill` declares the index first, so every row enters it by the write
//! path in random value order: leaves split and stay part full until the
//! repack packs them. `load` mixes `HSET r:<k> v <x>` (moves an index
//! entry), `HGET r:<k> p` and `IDX.QUERY rt RANGE lo hi LIMIT n`, and
//! writes one JSON object: per command, the latency histogram and its
//! percentiles, and how many requests were slower than `slow_us` in each
//! second of the window.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const CMDS: [&str; 3] = ["write", "read", "query"];
/// Sixteen buckets an octave of nanoseconds: about 4% wide.
const BUCKETS: usize = 32 + 36 * 16;

fn main() {
    let mut argv = std::env::args().skip(1);
    let mode = argv.next().unwrap_or_default();
    let kv: HashMap<String, String> = argv
        .filter_map(|a| a.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect();
    let get = |k: &str, d: u64| kv.get(k).map_or(d, |v| v.parse().expect("a number"));
    let port = get("port", 0) as u16;
    let rows = get("rows", 0);
    match mode.as_str() {
        "ping" => ping(port),
        "fill" => fill(port, rows, get("conns", 4), get("pad", 32) as usize, get("seed", 1)),
        "verify" => println!("{}", verify(port)),
        "load" => {
            let mix = kv.get("mix").map_or("30,60,10", String::as_str);
            let mix: Vec<u64> = mix.split(',').map(|x| x.parse().expect("mix=w,r,q")).collect();
            let p = LoadPlan {
                port,
                rows,
                secs: get("secs", 30),
                rate: get("rate", 30_000),
                conns: get("conns", 6),
                mix: [mix[0], mix[1], mix[2]],
                limit: get("limit", 10),
                slow_ns: get("slow_us", 300) * 1000,
                spin: get("spin", 1) == 1,
                seed: get("seed", 1),
            };
            let out = kv.get("out").expect("out=FILE");
            std::fs::write(out, load(&p)).expect("write the result file");
        }
        _ => {
            eprintln!("usage: repack_load ping|fill|verify|load port=P ...");
            std::process::exit(2);
        }
    }
}

struct Conn {
    s: TcpStream,
    buf: Vec<u8>,
    at: usize,
}

enum Reply {
    Ok,
    Err(String),
    Int(i64),
    Bulk(Vec<u8>),
    Arr(Vec<Reply>),
}

impl Conn {
    fn open(port: u16) -> std::io::Result<Conn> {
        let s = TcpStream::connect(("127.0.0.1", port))?;
        s.set_nodelay(true)?;
        Ok(Conn { s, buf: Vec::with_capacity(1 << 16), at: 0 })
    }

    fn send(&mut self, frame: &[u8]) {
        self.s.write_all(frame).expect("send");
    }

    fn line(&mut self) -> Vec<u8> {
        loop {
            if let Some(i) = self.buf[self.at..].windows(2).position(|w| w == b"\r\n") {
                let l = self.buf[self.at..self.at + i].to_vec();
                self.at += i + 2;
                return l;
            }
            self.more();
        }
    }

    fn more(&mut self) {
        if self.at > 0 {
            self.buf.drain(..self.at);
            self.at = 0;
        }
        let mut chunk = [0u8; 1 << 16];
        let n = self.s.read(&mut chunk).expect("read");
        assert!(n > 0, "server closed the connection");
        self.buf.extend_from_slice(&chunk[..n]);
    }

    fn reply(&mut self) -> Reply {
        let l = self.line();
        let body = String::from_utf8_lossy(&l[1..]).into_owned();
        match l[0] {
            b'+' => Reply::Ok,
            b'-' => Reply::Err(body),
            b':' => Reply::Int(body.parse().expect("an integer reply")),
            b'$' | b'=' | b'!' => {
                let n: i64 = body.parse().expect("a length");
                if n < 0 {
                    return Reply::Bulk(Vec::new());
                }
                while self.buf.len() - self.at < n as usize + 2 {
                    self.more();
                }
                let b = self.buf[self.at..self.at + n as usize].to_vec();
                self.at += n as usize + 2;
                Reply::Bulk(b)
            }
            b'*' | b'%' | b'~' => {
                let n: i64 = body.parse().expect("a count");
                let n = if l[0] == b'%' { n * 2 } else { n };
                Reply::Arr((0..n.max(0)).map(|_| self.reply()).collect())
            }
            _ => Reply::Ok,
        }
    }

    fn call(&mut self, parts: &[&[u8]]) -> Reply {
        let mut f = Vec::new();
        frame(&mut f, parts);
        self.send(&f);
        self.reply()
    }
}

fn frame(out: &mut Vec<u8>, parts: &[&[u8]]) {
    out.extend_from_slice(format!("*{}\r\n", parts.len()).as_bytes());
    for p in parts {
        out.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        out.extend_from_slice(p);
        out.extend_from_slice(b"\r\n");
    }
}

fn xorshift(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

/// Values spread over sixteen times the row count, so ranges are sparse
/// and a moved entry lands anywhere in the tree.
fn value_space(rows: u64) -> u64 {
    rows.max(1) * 16
}

fn ping(port: u16) {
    for _ in 0..100 {
        if let Ok(mut c) = Conn::open(port)
            && matches!(c.call(&[b"PING"]), Reply::Ok)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("repack_load: no server on port {port}");
    std::process::exit(1);
}

fn fill(port: u16, rows: u64, conns: u64, pad: usize, seed: u64) {
    let mut c = Conn::open(port).expect("connect");
    let parts: [&[u8]; 11] = [
        b"IDX.CREATE",
        b"rt",
        b"ON",
        b"PREFIX",
        b"r:",
        b"FIELD",
        b"v",
        b"TYPE",
        b"i64",
        b"KIND",
        b"range",
    ];
    if let Reply::Err(e) = c.call(&parts) {
        panic!("IDX.CREATE: {e}");
    }
    while let Reply::Err(e) = c.call(&[b"IDX.QUERY", b"rt", b"RANGE", b"0", b"1", b"LIMIT", b"1"]) {
        assert!(e.starts_with("INDEXBUILDING"), "IDX.QUERY: {e}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let t0 = Instant::now();
    let workers: Vec<_> = (0..conns)
        .map(|w| std::thread::spawn(move || fill_part(port, rows, conns, w, pad, seed)))
        .collect();
    for w in workers {
        w.join().expect("a fill worker");
    }
    println!("fill rows={rows} secs={:.2} {}", t0.elapsed().as_secs_f64(), verify(port));
}

/// Rows `w`, `w + conns`, `w + 2 conns`, … pipelined a thousand at a time.
fn fill_part(port: u16, rows: u64, conns: u64, w: u64, pad: usize, seed: u64) {
    let mut c = Conn::open(port).expect("connect");
    let mut x = (seed + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (w + 1);
    let padding = vec![b'x'; pad];
    let (mut batch, mut n) = (Vec::with_capacity(1 << 20), 0);
    let mut i = w;
    while i < rows {
        let (k, v) = (format!("r:{i}"), (xorshift(&mut x) % value_space(rows)).to_string());
        frame(&mut batch, &[b"HSET", k.as_bytes(), b"v", v.as_bytes(), b"p", &padding]);
        n += 1;
        i += conns;
        if n == 1000 || i >= rows {
            c.send(&batch);
            for _ in 0..n {
                if let Reply::Err(e) = c.reply() {
                    panic!("HSET: {e}");
                }
            }
            (n, batch) = (0, Vec::with_capacity(1 << 20));
        }
    }
}

/// `entries=<n> bytes=<n>` from IDX.VERIFY.
fn verify(port: u16) -> String {
    let mut c = Conn::open(port).expect("connect");
    let Reply::Arr(kv) = c.call(&[b"IDX.VERIFY", b"rt"]) else {
        return "entries=? bytes=?".into();
    };
    let text = |r: &Reply| match r {
        Reply::Bulk(b) => String::from_utf8_lossy(b).into_owned(),
        Reply::Int(i) => i.to_string(),
        _ => String::new(),
    };
    let mut out = HashMap::new();
    for pair in kv.chunks(2) {
        if let [k, v] = pair {
            out.insert(text(k), text(v));
        }
    }
    let field = |k: &str| out.get(k).cloned().unwrap_or_else(|| "?".into());
    format!("entries={} bytes={}", field("entries"), field("bytes"))
}

struct LoadPlan {
    port: u16,
    rows: u64,
    secs: u64,
    rate: u64,
    conns: u64,
    mix: [u64; 3],
    limit: u64,
    slow_ns: u64,
    spin: bool,
    seed: u64,
}

/// One connection's tallies.
struct Tally {
    hist: Vec<[u64; BUCKETS]>,
    service: Vec<[u64; BUCKETS]>,
    max_ns: [u64; 3],
    slow: Vec<[u64; 3]>,
    errors: u64,
    sent: u64,
}

fn load(p: &LoadPlan) -> String {
    let before = verify(p.port);
    let start = Instant::now() + Duration::from_millis(200);
    let tallies: Vec<Tally> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..p.conns).map(|w| s.spawn(move || drive(p, w, start))).collect();
        hs.into_iter().map(|h| h.join().expect("a load worker")).collect()
    });
    let after = verify(p.port);
    report(p, &tallies, &before, &after)
}

fn drive(p: &LoadPlan, w: u64, start: Instant) -> Tally {
    let mut c = Conn::open(p.port).expect("connect");
    let mut t = Tally {
        hist: vec![[0; BUCKETS]; 3],
        service: vec![[0; BUCKETS]; 3],
        max_ns: [0; 3],
        slow: vec![[0; 3]; p.secs as usize + 1],
        errors: 0,
        sent: 0,
    };
    let every = Duration::from_nanos(1_000_000_000 * p.conns / p.rate.max(1));
    let (mut x, total) =
        ((p.seed + 7).wrapping_mul(0x2545_F491_4F6C_DD1D) ^ (w + 1), p.mix.iter().sum::<u64>());
    let (mut due, end) =
        (start + every.mul_f64(w as f64 / p.conns as f64), start + Duration::from_secs(p.secs));
    let mut f = Vec::with_capacity(256);
    while due < end {
        wait_until(due, p.spin);
        let pick = xorshift(&mut x) % total;
        let cmd = if pick < p.mix[0] {
            0
        } else if pick < p.mix[0] + p.mix[1] {
            1
        } else {
            2
        };
        f.clear();
        request(&mut f, cmd, p, &mut x);
        let sent = Instant::now();
        c.send(&f);
        if let Reply::Err(e) = c.reply() {
            t.errors += 1;
            if t.errors == 1 {
                eprintln!("repack_load: {} answered -{e}", CMDS[cmd]);
            }
        }
        let done = Instant::now();
        let (lat, svc) = ((done - due).as_nanos() as u64, (done - sent).as_nanos() as u64);
        t.hist[cmd][bucket(lat)] += 1;
        t.service[cmd][bucket(svc)] += 1;
        t.max_ns[cmd] = t.max_ns[cmd].max(lat);
        if lat >= p.slow_ns {
            t.slow[((due - start).as_secs() as usize).min(p.secs as usize)][cmd] += 1;
        }
        t.sent += 1;
        due += every;
    }
    t
}

fn request(f: &mut Vec<u8>, cmd: usize, p: &LoadPlan, x: &mut u64) {
    let key = format!("r:{}", xorshift(x) % p.rows.max(1));
    let space = value_space(p.rows);
    match cmd {
        0 => {
            let v = (xorshift(x) % space).to_string();
            frame(f, &[b"HSET", key.as_bytes(), b"v", v.as_bytes()]);
        }
        1 => frame(f, &[b"HGET", key.as_bytes(), b"p"]),
        _ => {
            // a span holding about four times `limit` rows
            let span = 16 * 4 * p.limit.max(1);
            let lo = xorshift(x) % space.saturating_sub(span).max(1);
            let (a, b, n) = (lo.to_string(), (lo + span).to_string(), p.limit.to_string());
            frame(
                f,
                &[
                    b"IDX.QUERY",
                    b"rt",
                    b"RANGE",
                    a.as_bytes(),
                    b.as_bytes(),
                    b"LIMIT",
                    n.as_bytes(),
                ],
            );
        }
    }
}

fn wait_until(due: Instant, spin: bool) {
    let now = Instant::now();
    if due <= now {
        return;
    }
    let ahead = due - now;
    if !spin {
        std::thread::sleep(ahead);
        return;
    }
    if ahead > Duration::from_micros(1500) {
        std::thread::sleep(ahead - Duration::from_millis(1));
    }
    while Instant::now() < due {
        std::hint::spin_loop();
    }
}

fn bucket(ns: u64) -> usize {
    if ns < 32 {
        return ns as usize;
    }
    let e = 63 - ns.leading_zeros() as usize;
    (32 + (e - 5) * 16 + ((ns >> (e - 4)) & 15) as usize).min(BUCKETS - 1)
}

fn floor_ns(b: usize) -> u64 {
    if b < 32 { b as u64 } else { (16 + ((b - 32) % 16) as u64) << ((b - 32) / 16 + 1) }
}

fn quantile(h: &[u64; BUCKETS], q: f64) -> u64 {
    let n: u64 = h.iter().sum();
    let want = ((n as f64) * q).ceil().max(1.0) as u64;
    let mut seen = 0;
    for (b, c) in h.iter().enumerate() {
        seen += c;
        if seen >= want {
            return floor_ns(b);
        }
    }
    0
}

fn hist_json(h: &[u64; BUCKETS]) -> String {
    let cells: Vec<String> = h
        .iter()
        .enumerate()
        .filter(|(_, c)| **c > 0)
        .map(|(b, c)| format!("[{},{c}]", floor_ns(b)))
        .collect();
    format!("[{}]", cells.join(","))
}

fn summary(h: &[u64; BUCKETS], max_ns: u64) -> String {
    format!(
        "\"n\":{},\"p50_us\":{:.1},\"p99_us\":{:.1},\"p999_us\":{:.1},\"max_us\":{:.1}",
        h.iter().sum::<u64>(),
        quantile(h, 0.5) as f64 / 1e3,
        quantile(h, 0.99) as f64 / 1e3,
        quantile(h, 0.999) as f64 / 1e3,
        max_ns as f64 / 1e3,
    )
}

fn report(p: &LoadPlan, ts: &[Tally], before: &str, after: &str) -> String {
    let mut cmds = Vec::new();
    for (i, name) in CMDS.iter().enumerate() {
        let (mut h, mut s, mut max) = ([0u64; BUCKETS], [0u64; BUCKETS], 0);
        for t in ts {
            (0..BUCKETS).for_each(|b| (h[b], s[b]) = (h[b] + t.hist[i][b], s[b] + t.service[i][b]));
            max = max.max(t.max_ns[i]);
        }
        let slow: Vec<String> = (0..=p.secs as usize)
            .map(|sec| ts.iter().map(|t| t.slow[sec][i]).sum::<u64>().to_string())
            .collect();
        println!("  {name:<5} {}", summary(&h, max).replace('"', ""));
        cmds.push(format!(
            "\"{name}\":{{{},\"service_p999_us\":{:.1},\"hist\":{},\"service_hist\":{},\"slow_per_sec\":[{}]}}",
            summary(&h, max),
            quantile(&s, 0.999) as f64 / 1e3,
            hist_json(&h),
            hist_json(&s),
            slow.join(",")
        ));
    }
    let (sent, errors) =
        (ts.iter().map(|t| t.sent).sum::<u64>(), ts.iter().map(|t| t.errors).sum::<u64>());
    println!("  sent={sent} errors={errors} index before: {before} after: {after}");
    format!(
        "{{\"rows\":{},\"secs\":{},\"rate\":{},\"conns\":{},\"mix\":[{},{},{}],\"slow_us\":{},\"sent\":{sent},\
         \"errors\":{errors},\"index_before\":\"{before}\",\"index_after\":\"{after}\",\"cmds\":{{{}}}}}\n",
        p.rows,
        p.secs,
        p.rate,
        p.conns,
        p.mix[0],
        p.mix[1],
        p.mix[2],
        p.slow_ns / 1000,
        cmds.join(",")
    )
}

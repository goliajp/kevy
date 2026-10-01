//! A closed-loop pipelined load generator for benchmarking any RESP server.
//!
//! ```text
//! cargo run --release -p kevy --example loadgen -- \
//!     --port 6004 --threads 8 --conns 50 --pipe 16 --keyspace 1000000 -- SET key:__rand_int__ v
//! ```
//!
//! It exists because `redis-benchmark`, given the same eight load cores as
//! the server's four, could not keep kevy busy: the arena marked kevy's
//! cells LOAD-BOUND, which makes them floors instead of measurements. This
//! sends the same requests with less work per request: each command is
//! encoded once, `__rand_int__` (twelve bytes) is overwritten in place with a
//! twelve-digit key, and replies are counted, not parsed into values.
//!
//! Each thread owns its share of the connections and keeps one batch of
//! `--pipe` commands in flight on each: when a connection's replies are
//! in, its next batch goes out. It runs until killed, or until `--requests` have
//! been answered. It prints nothing while it runs; the server's own command
//! counter is the measurement.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const RAND: &[u8] = b"__rand_int__";

struct Opts {
    host: String,
    port: u16,
    threads: usize,
    conns: usize,
    pipe: usize,
    keyspace: u64,
    requests: u64,
    argv: Vec<String>,
}

fn parse() -> Opts {
    let mut o = Opts {
        host: "127.0.0.1".into(),
        port: 6004,
        threads: 1,
        conns: 1,
        pipe: 1,
        keyspace: 1,
        requests: u64::MAX,
        argv: Vec::new(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--" {
            o.argv = args.by_ref().collect();
            break;
        }
        let v = args.next().unwrap_or_else(|| usage(&format!("{a} needs a value")));
        let n = || v.parse::<u64>().unwrap_or_else(|_| usage(&format!("{a}: not a number: {v}")));
        match a.as_str() {
            "--host" => o.host = v.clone(),
            "--port" => o.port = n() as u16,
            "--threads" => o.threads = n() as usize,
            "--conns" => o.conns = n() as usize,
            "--pipe" => o.pipe = n() as usize,
            "--keyspace" => o.keyspace = n(),
            "--requests" => o.requests = n(),
            _ => usage(&format!("unknown option {a}")),
        }
    }
    if o.argv.is_empty() || o.threads == 0 || o.conns < o.threads || o.pipe == 0 || o.keyspace == 0
    {
        usage("needs a command after --, threads ≥ 1, conns ≥ threads, pipe ≥ 1, keyspace ≥ 1");
    }
    o
}

fn usage(why: &str) -> ! {
    eprintln!("loadgen: {why}");
    eprintln!("usage: loadgen [--host H] [--port P] [--threads T] [--conns C] [--pipe N]");
    eprintln!("               [--keyspace R] [--requests N] -- COMMAND ARG…");
    std::process::exit(2)
}

/// One command, RESP-encoded, and where each `__rand_int__` sits in it.
struct Template {
    bytes: Vec<u8>,
    holes: Vec<usize>,
}

impl Template {
    fn new(argv: &[String]) -> Template {
        let mut bytes = format!("*{}\r\n", argv.len()).into_bytes();
        let mut holes = Vec::new();
        for a in argv {
            bytes.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
            let start = bytes.len();
            bytes.extend_from_slice(a.as_bytes());
            let mut i = 0;
            while let Some(at) = a.as_bytes()[i..].windows(RAND.len()).position(|w| w == RAND) {
                holes.push(start + i + at);
                i += at + RAND.len();
            }
            bytes.extend_from_slice(b"\r\n");
        }
        Template { bytes, holes }
    }

    /// Append `n` commands to `out`, each hole filled from `rng`.
    fn render(&self, n: usize, keyspace: u64, rng: &mut u64, out: &mut Vec<u8>) {
        for _ in 0..n {
            let at = out.len();
            out.extend_from_slice(&self.bytes);
            for &h in &self.holes {
                let mut k = next(rng) % keyspace;
                for d in out[at + h..at + h + RAND.len()].iter_mut().rev() {
                    *d = b'0' + (k % 10) as u8;
                    k /= 10;
                }
            }
        }
    }
}

fn next(s: &mut u64) -> u64 {
    // xorshift64*: enough to spread keys, no dependency
    *s ^= *s >> 12;
    *s ^= *s << 25;
    *s ^= *s >> 27;
    s.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

/// Counts complete replies in a byte stream without keeping their values.
#[derive(Default)]
struct Replies {
    buf: Vec<u8>,
    /// Items the reply being read still needs (array elements included).
    owed: u64,
}

impl Replies {
    /// Feed bytes; returns how many replies completed.
    fn feed(&mut self, bytes: &[u8]) -> usize {
        self.buf.extend_from_slice(bytes);
        let mut done = 0;
        let mut at = 0;
        while let Some((used, elements)) = item(&self.buf[at..]) {
            at += used;
            self.owed = self.owed.max(1) - 1 + elements;
            if self.owed == 0 {
                done += 1;
            }
        }
        self.buf.drain(..at);
        done
    }
}

/// One RESP item at the front of `b`: its length and, for an array header,
/// how many elements follow; `None` while it is not all there.
fn item(b: &[u8]) -> Option<(usize, u64)> {
    let eol = b.windows(2).position(|w| w == b"\r\n")?;
    let num = || std::str::from_utf8(&b[1..eol]).ok()?.parse::<i64>().ok();
    match b.first()? {
        b'$' => match num()? {
            n if n < 0 => Some((eol + 2, 0)),
            n => {
                let end = eol + 2 + n as usize + 2;
                (b.len() >= end).then_some((end, 0))
            }
        },
        b'*' => Some((eol + 2, num()?.max(0) as u64)),
        _ => Some((eol + 2, 0)),
    }
}

fn run_thread(o: &Opts, conns: usize, seed: u64, answered: &AtomicU64) {
    let tpl = Template::new(&o.argv);
    let mut socks: Vec<(TcpStream, Replies)> = (0..conns)
        .map(|_| {
            let s = TcpStream::connect((o.host.as_str(), o.port)).expect("connect");
            s.set_nodelay(true).expect("nodelay");
            (s, Replies::default())
        })
        .collect();
    let mut rng = seed | 1;
    let mut out = Vec::with_capacity(tpl.bytes.len() * o.pipe);
    let mut inbuf = vec![0u8; 64 * 1024];
    // every connection keeps one batch in flight: as soon as a batch's
    // replies are in, the next batch goes out on that connection
    for (s, _) in &mut socks {
        out.clear();
        tpl.render(o.pipe, o.keyspace, &mut rng, &mut out);
        s.write_all(&out).expect("write");
    }
    while answered.load(Ordering::Relaxed) < o.requests {
        for (s, r) in &mut socks {
            let mut got = 0;
            while got < o.pipe {
                let n = s.read(&mut inbuf).expect("read");
                assert!(n > 0, "server closed the connection");
                got += r.feed(&inbuf[..n]);
            }
            out.clear();
            tpl.render(o.pipe, o.keyspace, &mut rng, &mut out);
            s.write_all(&out).expect("write");
        }
        answered.fetch_add((o.pipe * socks.len()) as u64, Ordering::Relaxed);
    }
}

fn main() {
    let o = Arc::new(parse());
    let answered = Arc::new(AtomicU64::new(0));
    let handles: Vec<_> = (0..o.threads)
        .map(|t| {
            let (o, answered) = (Arc::clone(&o), Arc::clone(&answered));
            let conns = o.conns / o.threads + usize::from(t < o.conns % o.threads);
            std::thread::spawn(move || {
                run_thread(&o, conns, 0x9e37_79b9 * (t as u64 + 1), &answered)
            })
        })
        .collect();
    for h in handles {
        h.join().expect("load thread");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_counted_across_split_reads() {
        let mut r = Replies::default();
        assert_eq!(r.feed(b"+OK\r\n:12\r\n$3\r\nab"), 2);
        assert_eq!(r.feed(b"c\r\n$-1\r\n-ERR x\r\n"), 3);
        // an array is one reply, however its elements arrive
        assert_eq!(r.feed(b"*2\r\n$1\r\na\r\n*1\r\n:1"), 0);
        assert_eq!(r.feed(b"\r\n*0\r\n"), 2);
    }

    #[test]
    fn a_hole_is_filled_with_twelve_digits() {
        let t = Template::new(&["SET".into(), "k:__rand_int__".into(), "v".into()]);
        let mut out = Vec::new();
        t.render(1, 1000, &mut 7, &mut out);
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("*3\r\n$3\r\nSET\r\n$14\r\nk:000000000"), "{s}");
        assert_eq!(s.len(), t.bytes.len());
    }
}

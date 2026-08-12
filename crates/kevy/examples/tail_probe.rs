//! tail_probe — the in-process PING prober tailgate runs.
//!
//! One paced connection: a PING every millisecond, each RTT recorded,
//! percentiles at the end, plus the server's own
//! `reactor_tick_gap_max_us` gauge so the two views (client-observed
//! tail, reactor-observed stall) print side by side. In-process
//! because the balance round's lesson says subprocess monitors
//! self-pollute the very gaps they count.
//!
//!   tail_probe <port> <seconds>
//!
//! Output (one line, machine-readable):
//!   tail-probe: n=N p50us=A p99us=B p999us=C maxus=D reactor_gap_us=E

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().and_then(|s| s.parse().ok()).expect("usage: tail_probe <port> <seconds>");
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).expect("usage: tail_probe <port> <seconds>");

    let mut conn = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    conn.set_nodelay(true).expect("nodelay");
    let mut buf = [0u8; 512];

    // Tick cadence is a RATE, so it needs two reads over a known
    // window. The gap gauge alone is a high-water mark: one late tick
    // and a chronically starved loop print the same number.
    let ticks_before = read_stat(&mut conn, "reactor_ticks_total:");
    let window_start = Instant::now();
    let deadline = window_start + Duration::from_secs(secs);
    let mut rtts_us: Vec<u64> = Vec::with_capacity((secs as usize) * 1100);
    while Instant::now() < deadline {
        let t0 = Instant::now();
        conn.write_all(b"*1\r\n$4\r\nPING\r\n").expect("write");
        // Read until the CRLF — under load the kernel may hand the
        // 7-byte reply in pieces (round 2 of the first box run
        // panicked exactly there).
        let mut got = 0usize;
        loop {
            let n = conn.read(&mut buf[got..]).expect("read");
            assert!(n > 0, "server closed mid-reply");
            got += n;
            if buf[..got].ends_with(b"\r\n") {
                break;
            }
        }
        assert!(&buf[..got] == b"+PONG\r\n", "unexpected reply: {:?}", &buf[..got]);
        let rtt = t0.elapsed();
        rtts_us.push(rtt.as_micros() as u64);
        // Pace to ~1 kHz so the prober measures the server, not itself;
        // a stalled reply self-paces (no catch-up bursts that would
        // count one stall many times).
        if let Some(rest) = Duration::from_millis(1).checked_sub(rtt) {
            std::thread::sleep(rest);
        }
    }

    rtts_us.sort_unstable();
    let pct = |p: f64| -> u64 {
        let idx = ((rtts_us.len() as f64) * p).ceil() as usize;
        rtts_us[idx.clamp(1, rtts_us.len()) - 1]
    };
    let elapsed = window_start.elapsed();
    let reactor_gap = read_stat(&mut conn, "reactor_tick_gap_max_us:");
    let ticks = read_stat(&mut conn, "reactor_ticks_total:").saturating_sub(ticks_before);
    // Instance-wide ticks per second: every shard ticks on its own
    // clock, so this is the housekeeping rate of the server as a whole.
    let tick_hz = (ticks as f64) / elapsed.as_secs_f64();
    println!(
        "tail-probe: n={} p50us={} p99us={} p999us={} maxus={} reactor_gap_us={} \
ticks={} tick_hz={tick_hz:.1}",
        rtts_us.len(),
        pct(0.50),
        pct(0.99),
        pct(0.999),
        rtts_us.last().copied().unwrap_or(0),
        reactor_gap,
        ticks,
    );
}

/// `INFO stats` → one named `u64` line, so the report carries both the
/// client view and the server's self-observation.
fn read_stat(conn: &mut TcpStream, key: &str) -> u64 {
    conn.write_all(b"*2\r\n$4\r\nINFO\r\n$5\r\nstats\r\n").expect("write INFO");
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = conn.read(&mut buf).expect("read INFO");
        out.extend_from_slice(&buf[..n]);
        // The bulk reply is complete once the payload matches its
        // declared length; a lazy check that suffices here: stop when
        // the terminator arrives and the gauge line is present.
        if out.ends_with(b"\r\n") {
            let text = String::from_utf8_lossy(&out);
            if let Some(line) = text.lines().find(|l| l.starts_with(key)) {
                return line.split(':').nth(1).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
            }
        }
        if n == 0 {
            return 0;
        }
    }
}

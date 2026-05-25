//! kevy-pubsub-bench — a pub/sub fan-out throughput load generator.
//!
//! `valkey-benchmark` has no pub/sub mode, so this measures the metric directly:
//! `--subs K` subscribers all SUBSCRIBE one channel, then a publisher floods
//! `--msgs M` PUBLISHes; we time how long until every subscriber has received all
//! M messages. The headline number is **delivered messages/sec = K·M / elapsed**
//! (the fan-out the server actually did), plus the publish rate M / elapsed.
//!
//! Pure Rust, zero deps, raw RESP over TCP — works against kevy, valkey, or redis.
//!
//! Usage: `kevy-pubsub-bench --host H --port P --subs K --msgs M --size S`

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

const CHANNEL: &str = "bench";

fn arg(name: &str, default: &str) -> String {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

/// Encode a RESP array of bulk strings.
fn resp(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

/// Read and discard exactly one RESP reply (recursing into arrays).
fn read_reply(s: &mut impl Read) -> std::io::Result<()> {
    let tag = read_byte(s)?;
    match tag {
        b'+' | b'-' | b':' => {
            read_line(s)?;
        }
        b'$' => {
            let n = read_line(s)?;
            if let Ok(len) = n.trim().parse::<i64>()
                && len >= 0
            {
                let mut buf = vec![0u8; len as usize + 2]; // payload + CRLF
                s.read_exact(&mut buf)?;
            }
        }
        b'*' => {
            let n = read_line(s)?;
            if let Ok(cnt) = n.trim().parse::<i64>() {
                for _ in 0..cnt.max(0) {
                    read_reply(s)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn read_byte(s: &mut impl Read) -> std::io::Result<u8> {
    let mut b = [0u8; 1];
    s.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_line(s: &mut impl Read) -> std::io::Result<String> {
    let mut out = Vec::new();
    loop {
        let b = read_byte(s)?;
        if b == b'\r' {
            let _ = read_byte(s)?; // consume \n
            break;
        }
        out.push(b);
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn main() {
    let host = arg("--host", "127.0.0.1");
    let port: u16 = arg("--port", "6379").parse().unwrap();
    let subs: usize = arg("--subs", "50").parse().unwrap();
    let msgs: usize = arg("--msgs", "100000").parse().unwrap();
    let size: usize = arg("--size", "16").parse().unwrap();
    let addr = format!("{host}:{port}");

    // One message frame's byte length (all messages are identical, so a
    // subscriber is done once it has read msgs × this many bytes).
    let payload = vec![b'x'; size];
    let msg_frame = resp(&[b"message", CHANNEL.as_bytes(), &payload]).len();
    let target_bytes = msgs * msg_frame;

    let ready = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));

    let mut subscribers = Vec::with_capacity(subs);
    for _ in 0..subs {
        let addr = addr.clone();
        let ready = ready.clone();
        let done = done.clone();
        subscribers.push(std::thread::spawn(move || {
            let mut s = TcpStream::connect(&addr).expect("subscriber connect");
            s.set_nodelay(true).ok();
            s.write_all(&resp(&[b"SUBSCRIBE", CHANNEL.as_bytes()]))
                .unwrap();
            read_reply(&mut s).unwrap(); // subscribe confirmation
            ready.fetch_add(1, Ordering::SeqCst);
            // Drain exactly `target_bytes` of message frames.
            let mut buf = vec![0u8; 256 * 1024];
            let mut got = 0usize;
            while got < target_bytes {
                match s.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => got += n,
                    Err(_) => break,
                }
            }
            done.fetch_add(1, Ordering::SeqCst);
        }));
    }

    // Wait until every subscriber is subscribed before publishing.
    while ready.load(Ordering::SeqCst) < subs {
        std::thread::yield_now();
    }

    let mut p = TcpStream::connect(&addr).expect("publisher connect");
    p.set_nodelay(true).ok();
    let pub_frame = resp(&[b"PUBLISH", CHANNEL.as_bytes(), &payload]);

    let start = Instant::now();
    // Pipeline in batches so the publisher's reply backlog can't fill the socket
    // and deadlock the write side.
    const BATCH: usize = 1024;
    let mut sent = 0usize;
    while sent < msgs {
        let n = BATCH.min(msgs - sent);
        let mut req = Vec::with_capacity(n * pub_frame.len());
        for _ in 0..n {
            req.extend_from_slice(&pub_frame);
        }
        p.write_all(&req).unwrap();
        for _ in 0..n {
            read_reply(&mut p).unwrap(); // :<subscriber count>
        }
        sent += n;
    }
    // Wait for all subscribers to receive everything.
    while done.load(Ordering::SeqCst) < subs {
        std::thread::yield_now();
    }
    let elapsed = start.elapsed();
    for h in subscribers {
        let _ = h.join();
    }

    let secs = elapsed.as_secs_f64();
    let delivered = (subs * msgs) as f64;
    let publishes = msgs as f64;
    println!(
        "pubsub host={addr} subs={subs} msgs={msgs} size={size}B  \
         delivered={:.0} msg/s  publishes={:.0}/s  elapsed={:.3}s",
        delivered / secs,
        publishes / secs,
        secs,
    );
}

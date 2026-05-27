//! Cross-competitor Rust RESP parser bench. Compares kevy-resp against
//! `redis-rs`'s in-tree parser (`redis::parse_redis_value`), on a
//! representative command frame and a representative reply frame.

use kevy_resp::parse_command;
use std::hint::black_box;
use std::time::Instant;

const ITER: usize = 1_000_000;
const SAMPLES: usize = 25;
const HOST: &str = "M4-Pro-aarch64";
const STONE: &str = "kevy-resp";

fn now_iso() -> String {
    std::process::Command::new("date")
        .args(["-u", "-Iseconds"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn percentiles(times: &mut Vec<u64>) -> (u64, u64, u64) {
    times.sort_unstable();
    let n = times.len();
    (times[n / 2], times[(n * 95) / 100], times[0])
}

fn emit(competitor: &str, workload: &str, m: u64, p95: u64, min: u64) {
    println!(
        "{{\"stone\":\"{STONE}\",\"language\":\"rust\",\"competitor\":\"{competitor}\",\"workload\":\"{workload}\",\"metric\":\"ns_per_op\",\"value_median\":{m},\"value_p95\":{p95},\"value_min\":{min},\"iterations\":{ITER},\"host\":\"{HOST}\",\"date\":\"{}\"}}",
        now_iso()
    );
}

fn time_one<F: FnMut()>(iter: usize, mut f: F) -> u64 {
    let t = Instant::now();
    for _ in 0..iter {
        f();
    }
    (t.elapsed().as_nanos() as u64) / iter as u64
}

fn bench<F: FnMut()>(competitor: &str, workload: &str, mut f: F) {
    let mut times = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        times.push(time_one(ITER, &mut f));
    }
    let (m, p95, min) = percentiles(&mut times);
    emit(competitor, workload, m, p95, min);
}

fn main() {
    // Representative request: SET key value
    let set_cmd: &[u8] = b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nvalue\r\n";
    // Representative reply: bulk "hello world!" (12 bytes)
    let bulk_reply: &[u8] = b"$12\r\nhello world!\r\n";

    // ---- parse_command (kevy-resp) vs redis-rs parse_redis_value (request) ----
    bench("kevy-resp parse_command", "parse_command_set_3args", || {
        black_box(parse_command(black_box(set_cmd)).unwrap());
    });
    bench("redis-rs parse_redis_value", "parse_command_set_3args", || {
        // redis-rs's parser; same frame shape, returns Value::Bulk(Vec<Value>)
        let _ = black_box(redis::parse_redis_value(black_box(set_cmd)).unwrap());
    });

    // ---- parse_reply (bulk string) ----
    bench("kevy-resp parse_reply", "parse_reply_bulk_12B", || {
        black_box(kevy_resp::parse_reply(black_box(bulk_reply)).unwrap());
    });
    bench("redis-rs parse_redis_value", "parse_reply_bulk_12B", || {
        let _ = black_box(redis::parse_redis_value(black_box(bulk_reply)).unwrap());
    });
}

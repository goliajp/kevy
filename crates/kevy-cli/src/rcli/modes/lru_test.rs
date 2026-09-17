//! `--lru-test <keys>`: an 80/20 read and write load over `keys` keys, with
//! the hit rate every second — how an eviction policy holds up.

use super::random::Random;
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};
use kevy_resp::Reply;
use std::time::{Duration, Instant};

const BATCH: usize = 250;

/// Run until stopped; 1 on a connection failure.
pub(crate) fn run(s: &mut Session, keys: i64) -> u8 {
    let mut random = Random::seeded();
    loop {
        let started = Instant::now();
        let (mut hits, mut misses) = (0u64, 0u64);
        while started.elapsed() < Duration::from_secs(1) {
            let writes: Vec<(Vec<u8>, Vec<u8>)> =
                (0..BATCH).map(|_| (key(&mut random, keys), value(&mut random))).collect();
            let sets: Vec<Vec<&[u8]>> =
                writes.iter().map(|(k, v)| vec![&b"SET"[..], k, v]).collect();
            let reads: Vec<Vec<u8>> = (0..BATCH).map(|_| key(&mut random, keys)).collect();
            let gets: Vec<Vec<&[u8]>> = reads.iter().map(|k| vec![&b"GET"[..], k]).collect();
            let Ok(replies) = s.pipeline(&sets).and_then(|_| s.pipeline(&gets)) else {
                eprint_bytes(&[b"I/O error during LRU test\n"]);
                return 1;
            };
            for reply in replies {
                match reply {
                    Reply::Error(msg) => eprint_bytes(&[&msg, b"\n"]),
                    Reply::Nil | Reply::Null => misses += 1,
                    _ => hits += 1,
                }
            }
        }
        let gets = hits + misses;
        let share = |n: u64| n as f64 / gets as f64 * 100.0;
        let line = format!(
            "{gets} Gets/sec | Hits: {hits} ({:.2}%) | Misses: {misses} ({:.2}%)\n",
            share(hits),
            share(misses)
        );
        write_out(line.as_bytes());
    }
}

/// `lru:<n>` for `n` in `1..=keys`, lower numbers far more often: with this
/// exponent 20% of the keys take 80% of the traffic.
fn key(random: &mut Random, keys: i64) -> Vec<u8> {
    const ALPHA: f64 = 6.2;
    let (min, max) = (1.0f64, keys as f64 + 1.0);
    let r = random.unit();
    let spread = (max.powf(ALPHA + 1.0) - min.powf(ALPHA + 1.0)) * r + min.powf(ALPHA + 1.0);
    let drawn = spread.powf(1.0 / (ALPHA + 1.0)) as i64;
    format!("lru:{}", (keys - drawn) + 1).into_bytes()
}

/// Five letters from `A` to `y`.
fn value(random: &mut Random) -> Vec<u8> {
    (0..5).map(|_| b'A' + (random.next_u64() % u64::from(b'z' - b'A')) as u8).collect()
}

#[cfg(test)]
mod tests {
    use super::{key, value};
    use crate::rcli::modes::random::Random;

    #[test]
    fn keys_favour_the_low_numbers() {
        let mut r = Random::seeded();
        let drawn: Vec<i64> = (0..10_000)
            .map(|_| String::from_utf8_lossy(&key(&mut r, 1000))[4..].parse::<i64>().unwrap_or(-1))
            .collect();
        assert!(drawn.iter().all(|n| (1..=1000).contains(n)), "in range");
        let low = drawn.iter().filter(|n| **n <= 200).count();
        assert!(low > 7_000, "{low} of 10000 in the lowest fifth");
        assert!(value(&mut r).iter().all(|b| (b'A'..b'z').contains(b)));
    }
}

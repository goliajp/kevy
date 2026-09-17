//! `--latency-dist`: how PING round trips spread over time buckets, one
//! coloured line per interval.

use crate::rcli::send::write_out;
use crate::rcli::session::Session;
use std::time::{Duration, Instant};

/// Upper bounds in microseconds and their symbols; past the last, `?`.
const BUCKETS: [(u64, u8); 30] = [
    (10, b'.'),
    (125, b'-'),
    (250, b'*'),
    (500, b'#'),
    (1_000, b'1'),
    (2_000, b'2'),
    (3_000, b'3'),
    (4_000, b'4'),
    (5_000, b'5'),
    (6_000, b'6'),
    (7_000, b'7'),
    (8_000, b'8'),
    (9_000, b'9'),
    (10_000, b'A'),
    (20_000, b'B'),
    (30_000, b'C'),
    (40_000, b'D'),
    (50_000, b'E'),
    (100_000, b'F'),
    (200_000, b'G'),
    (300_000, b'H'),
    (400_000, b'I'),
    (500_000, b'J'),
    (1_000_000, b'K'),
    (2_000_000, b'L'),
    (4_000_000, b'M'),
    (8_000_000, b'N'),
    (16_000_000, b'O'),
    (30_000_000, b'P'),
    (60_000_000, b'Q'),
];
const COLOR: [u8; 19] =
    [0, 233, 234, 235, 237, 239, 241, 243, 245, 247, 144, 143, 142, 184, 226, 214, 208, 202, 196];
const MONO: [u8; 13] = [0, 233, 234, 235, 237, 239, 241, 243, 245, 247, 249, 251, 253];

/// Measure until stopped.
pub(crate) fn run(s: &mut Session) -> u8 {
    let palette: &[u8] = if s.opts.modes.mono { &MONO } else { &COLOR };
    let window = Duration::from_millis(match s.opts.interval_us {
        0 => 1000,
        us => us / 1000,
    });
    let mut counts = [0u64; BUCKETS.len() + 1];
    let (mut started, mut shown) = (Instant::now(), 0u64);
    loop {
        let sent = Instant::now();
        let _ = s.request_reconnecting(&[b"PING"]); // only the round trip matters
        let micros = sent.elapsed().as_micros() as u64;
        let slot = BUCKETS.iter().position(|(max, _)| micros <= *max).unwrap_or(BUCKETS.len());
        counts[slot] += 1;
        if started.elapsed() > window {
            if shown.is_multiple_of(20) {
                write_out(&legend(palette));
            }
            shown += 1;
            write_out(&spectrum(&counts, palette));
            counts = [0; BUCKETS.len() + 1];
            started = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn legend(palette: &[u8]) -> Vec<u8> {
    let mut out = b"---------------------------------------------\n\
. - * #          .01 .125 .25 .5 milliseconds\n\
1,2,3,...,9      from 1 to 9     milliseconds\n\
A,B,C,D,E        10,20,30,40,50  milliseconds\n\
F,G,H,I,J        .1,.2,.3,.4,.5       seconds\n\
K,L,M,N,O,P,Q,?  1,2,4,8,16,30,60,>60 seconds\n\
From 0 to 100%: "
        .to_vec();
    for color in palette {
        out.extend_from_slice(format!("\x1b[48;5;{color}m ").as_bytes());
    }
    out.extend_from_slice(b"\x1b[0m\n---------------------------------------------\n");
    out
}

/// Each bucket's symbol on a background as bright as its share of samples.
fn spectrum(counts: &[u64], palette: &[u8]) -> Vec<u8> {
    let total: u64 = counts.iter().sum();
    let mut out = b"\x1b[38;5;0m".to_vec();
    let symbols = BUCKETS.iter().map(|(_, c)| *c).chain(std::iter::once(b'?'));
    for (count, symbol) in counts.iter().zip(symbols) {
        let index = (*count as f64 / total as f64 * (palette.len() - 1) as f64).ceil() as usize;
        out.extend_from_slice(
            format!("\x1b[48;5;{}m", palette[index.min(palette.len() - 1)]).as_bytes(),
        );
        out.push(symbol);
    }
    out.extend_from_slice(b"\x1b[0m\n");
    out
}

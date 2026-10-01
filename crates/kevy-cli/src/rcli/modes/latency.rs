//! `--latency` and `--latency-history`: PING round trips, in milliseconds.

use super::hdr::Histogram;
use crate::rcli::format::Output;
use crate::rcli::send::write_out;
use crate::rcli::session::Session;
use std::time::{Duration, Instant};

/// A PING every 10 ms.
const SAMPLE_EVERY: Duration = Duration::from_millis(10);

/// The readings of the current window, in microseconds.
struct Window {
    min: u64,
    max: u64,
    sum: u64,
    count: u64,
    histogram: Histogram,
    started: Instant,
}

impl Window {
    fn new() -> Window {
        Window {
            min: 0,
            max: 0,
            sum: 0,
            count: 0,
            histogram: Histogram::latencies(),
            started: Instant::now(),
        }
    }

    fn add(&mut self, micros: u64) {
        if self.count == 0 {
            (self.min, self.max) = (micros, micros);
        }
        self.min = self.min.min(micros);
        self.max = self.max.max(micros);
        self.sum += micros;
        self.count += 1;
        self.histogram.record(micros);
    }
}

/// Measure until stopped; without history, and not on a terminal, one line
/// after the interval and exit 0.
pub(crate) fn run(s: &mut Session) -> u8 {
    let history = s.opts.modes.latency_history;
    let interval_ms = match s.opts.interval_us {
        0 => None,
        us => Some(us / 1000),
    };
    let report_after = Duration::from_millis(interval_ms.unwrap_or(1000));
    let window_length = Duration::from_millis(interval_ms.unwrap_or(15_000));
    let standard = s.opts.output == Output::Standard;
    let mut w = Window::new();
    loop {
        let sent = Instant::now();
        let _ = s.request_reconnecting(&[b"PING"]); // only the round trip matters
        w.add(sent.elapsed().as_micros() as u64);
        let line = reading(s, &w);
        if standard {
            write_out(&[b"\x1b[0G\x1b[2K".as_slice(), &line].concat());
        } else if history {
            write_out(&[line.as_slice(), b"\n"].concat());
        } else if w.started.elapsed() > report_after {
            if !line.is_empty() {
                write_out(&[line.as_slice(), b"\n"].concat());
            }
            return 0;
        }
        if history && w.started.elapsed() > window_length {
            write_out(
                format!(" -- {:.2} seconds range\n", w.started.elapsed().as_secs_f32()).as_bytes(),
            );
            w = Window::new();
        }
        std::thread::sleep(SAMPLE_EVERY);
    }
}

/// The window's reading in the output format, without a line end; empty for
/// quoted JSON, which prints nothing.
fn reading(s: &Session, w: &Window) -> Vec<u8> {
    let ms = |micros: u64| micros as f64 / 1000.0;
    let avg = w.sum as f64 / w.count as f64 / 1000.0;
    let (min, max, count) = (ms(w.min), ms(w.max), w.count);
    let percentiles: Vec<(String, f64)> = s
        .opts
        .modes
        .latency_percentiles
        .iter()
        .map(|(p, label)| {
            (String::from_utf8_lossy(label).into_owned(), ms(w.histogram.value_at_percentile(*p)))
        })
        .collect();
    let list =
        |sep: &str| percentiles.iter().map(|(_, v)| format!("{sep}{v:.3}")).collect::<String>();
    match s.opts.output {
        Output::Standard => {
            let extra: String =
                percentiles.iter().map(|(l, v)| format!(", p{l}: {v:.3}")).collect();
            format!("min: {min:.3}, max: {max:.3}, avg: {avg:.3} ({count} samples){extra}")
        }
        Output::Csv => format!("{min:.3},{max:.3},{avg:.3},{count}{}", list(",")),
        Output::Raw => format!("{min:.3} {max:.3} {avg:.3} {count}{}", list(" ")),
        Output::Json => {
            let p = if percentiles.is_empty() {
                String::new()
            } else {
                let items: Vec<String> =
                    percentiles.iter().map(|(l, v)| format!("\"{l}\": {v:.3}")).collect();
                format!(", \"percentiles\": {{{}}}", items.join(", "))
            };
            format!(
                "{{\"min\": {min:.3}, \"max\": {max:.3}, \"avg\": {avg:.3}, \"count\": {count}{p}}}"
            )
        }
        Output::QuotedJson => String::new(),
    }
    .into_bytes()
}

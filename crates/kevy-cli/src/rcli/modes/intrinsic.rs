//! `--intrinsic-latency <seconds>`: how long this machine keeps a busy
//! process from running, with no network or server involved.

use crate::rcli::send::write_out;
use std::hint::black_box;
use std::time::{Duration, Instant};

/// Measure for `seconds` (or until Ctrl-C) and report; exit 0.
pub(crate) fn run(seconds: i32) -> u8 {
    kevy_sys::install_interrupt(1);
    kevy_sys::note_interrupts();
    let run_micros = f64::from(seconds) * 1_000_000.0;
    let end = Instant::now() + Duration::from_secs(u64::try_from(seconds).unwrap_or(0));
    let (mut worst, mut runs) = (0u128, 0u64);
    loop {
        let started = Instant::now();
        busy_work();
        let micros = started.elapsed().as_micros();
        runs += 1;
        if micros == 0 {
            continue;
        }
        if micros > worst {
            worst = micros;
            write_out(format!("Max latency so far: {worst} microseconds.\n").as_bytes());
        }
        if kevy_sys::take_noted() || Instant::now() > end {
            let avg = run_micros / runs as f64;
            write_out(
                format!(
                    "\n{runs} total runs (avg latency: {avg:.4} microseconds / {:.2} nanoseconds per run).\nWorst run took {:.0}x longer than the average latency.\n",
                    avg * 1e3,
                    worst as f64 / avg
                )
                .as_bytes(),
            );
            return 0;
        }
    }
}

/// A few microseconds of arithmetic the compiler cannot fold away.
fn busy_work() {
    let mut acc = 0u64;
    for i in 0..black_box(64u64) {
        acc = black_box(acc.wrapping_mul(31).wrapping_add(i ^ (acc >> 7)));
    }
    black_box(acc);
}

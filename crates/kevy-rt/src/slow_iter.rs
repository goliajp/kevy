//! Slow-iteration breakdown for the poll reactors — the opt-in
//! `KEVY_DEBUG_SLOW_ITER_MS` dump.
//!
//! The tick-gap gauge says a single loop iteration occasionally takes
//! half a second on epoll (the epoll tick-cadence findings in
//! `bench/`); it cannot say WHERE. This records coarse phase
//! boundaries and prints the breakdown for any iteration over the
//! threshold, so the seat is named by a measurement instead of a guess.
//!
//! Coarse on purpose. Timers around small steps are unreliable — an
//! optimizing build can sink stores past a clock read, and the SPG
//! sort-spill arc burned eight rounds on exactly that. These phases are
//! whole syscall-and-loop segments and the target is ~500 ms, so the
//! per-mark resolution is irrelevant to the verdict.
//!
//! Cost when off: one `bool` test per mark, no clock read at all.

use std::sync::OnceLock;
use std::time::Instant;

/// Phases in loop order. Fixed array, no allocation on the hot path.
const PHASES: usize = 16;

static THRESHOLD_MS: OnceLock<Option<u64>> = OnceLock::new();

/// The dump threshold in ms, or `None` when the feature is off.
fn threshold_ms() -> Option<u64> {
    *THRESHOLD_MS.get_or_init(|| {
        std::env::var("KEVY_DEBUG_SLOW_ITER_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
    })
}

/// One iteration's phase timings. Construct once per reactor (not per
/// iteration) and call [`Self::begin`] at the top of each pass.
pub(crate) struct SlowIter {
    threshold_us: Option<u64>,
    last: Instant,
    names: [&'static str; PHASES],
    took_us: [u64; PHASES],
    n: usize,
}

impl SlowIter {
    pub(crate) fn new() -> Self {
        Self {
            threshold_us: threshold_ms().map(|ms| ms * 1000),
            last: Instant::now(),
            names: [""; PHASES],
            took_us: [0; PHASES],
            n: 0,
        }
    }

    /// True when the dump is enabled — lets a caller skip work that
    /// only the dump needs.
    pub(crate) fn enabled(&self) -> bool {
        self.threshold_us.is_some()
    }

    /// Start a new iteration's measurement.
    #[inline]
    pub(crate) fn begin(&mut self) {
        if self.threshold_us.is_none() {
            return;
        }
        self.n = 0;
        self.last = Instant::now();
    }

    /// Close the phase that ends here.
    #[inline]
    pub(crate) fn mark(&mut self, name: &'static str) {
        if self.threshold_us.is_none() || self.n >= PHASES {
            return;
        }
        let now = Instant::now();
        self.names[self.n] = name;
        self.took_us[self.n] = now.duration_since(self.last).as_micros() as u64;
        self.n += 1;
        self.last = now;
    }

    /// Print the breakdown if this iteration ran over the threshold.
    /// `extra` carries whatever the caller can cheaply add about the
    /// iteration's shape (event count, whether the tick body ran).
    pub(crate) fn finish(&mut self, shard: usize, extra: std::fmt::Arguments<'_>) {
        let Some(limit_us) = self.threshold_us else {
            return;
        };
        let total: u64 = self.took_us[..self.n].iter().sum();
        if total < limit_us {
            return;
        }
        let mut line = String::with_capacity(160);
        for i in 0..self.n {
            if self.took_us[i] == 0 {
                continue; // sub-microsecond phases are noise at this scale
            }
            line.push_str(&format!(" {}={}us", self.names[i], self.took_us[i]));
        }
        eprintln!("kevy: [slowiter] shard {shard} total={total}us{line} | {extra}");
    }
}

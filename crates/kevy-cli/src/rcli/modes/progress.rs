//! A progress bar a terminal redraws in place, at most every 300 ms.

use crate::rcli::send::write_out;
use std::time::{Duration, Instant};

const WIDTH: usize = 60;
const EVERY: Duration = Duration::from_millis(300);

/// When the bar was last drawn.
pub(crate) struct Bar {
    drawn: Option<Instant>,
}

impl Bar {
    pub(crate) fn new() -> Bar {
        Bar { drawn: None }
    }

    /// Draw `block` if the last drawing is old enough.
    pub(crate) fn maybe_draw(&mut self, block: &[u8]) {
        if self.drawn.is_some_and(|t| t.elapsed() < EVERY) {
            return;
        }
        if self.drawn.is_some() {
            write_out(block);
        }
        self.drawn = Some(Instant::now());
    }
}

/// `100.00% ||||…` in green, the rest `-` in red.
pub(crate) fn bar_line(pct: f64) -> Vec<u8> {
    let filled = ((pct.clamp(0.0, 100.0) / 100.0) * WIDTH as f64) as usize;
    let mut out = format!("\x1b[2K\r{pct:6.2}% \x1b[32m").into_bytes();
    out.extend(std::iter::repeat_n(b'|', filled));
    out.extend_from_slice(b"\x1b[31m");
    out.extend(std::iter::repeat_n(b'-', WIDTH - filled));
    out.extend_from_slice(b"\x1b[39m\n");
    out
}

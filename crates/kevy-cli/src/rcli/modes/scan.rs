//! `--scan`: every key matching `--pattern`, one per line.

use super::pages::Pages;
use crate::rcli::format::Output;
use crate::rcli::session::{Session, eprint_bytes};

/// List the keys; exit 0 at the end or on Ctrl-C, 1 on a failed SCAN.
pub(crate) fn run(s: &mut Session) -> u8 {
    kevy_sys::install_interrupt(1);
    kevy_sys::note_interrupts();
    let quoted = s.opts.output == Output::Standard;
    let mut pages = Pages::new(0, s.opts.modes.pattern.clone(), s.opts.modes.count);
    loop {
        let keys = match pages.next(s) {
            Ok(Some(keys)) => keys,
            Ok(None) => return 0,
            Err(e) => {
                eprint_bytes(&[&e.text(), b"\n"]);
                return 1;
            }
        };
        let mut out = Vec::new();
        for key in &keys {
            // DEV-019: a key is printed whole; redis-cli stops at a NUL byte.
            out.extend_from_slice(&if quoted { crate::rcli::repr::repr(key) } else { key.clone() });
            out.push(b'\n');
        }
        crate::rcli::send::write_out(&out);
        if s.opts.interval_us > 0 {
            std::thread::sleep(std::time::Duration::from_micros(s.opts.interval_us));
        }
        if kevy_sys::take_noted() {
            return 0;
        }
    }
}

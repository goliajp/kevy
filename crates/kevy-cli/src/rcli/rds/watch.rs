//! `watch <seconds> [count] <command …>`: run a command again and again,
//! redrawing on a terminal and appending when piped.

use super::options::Common;
use crate::rcli::format::Output;
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};
use std::time::Duration;

/// Run `watch`; the exit code.
pub(crate) fn run(s: &mut Session, common: &Common) -> u8 {
    let seconds =
        common.args.first().and_then(|v| std::str::from_utf8(v).ok()?.parse::<f64>().ok());
    let Some(seconds) = seconds.filter(|s| *s > 0.0) else {
        eprint_bytes(&[b"kevy-cli: watch <seconds> [count] <command ...>\n"]);
        return 1;
    };
    let rest = &common.args[1..];
    let count = rest.first().and_then(|v| std::str::from_utf8(v).ok()?.parse::<u64>().ok());
    let command = if count.is_some() && rest.len() > 1 { &rest[1..] } else { rest };
    if command.is_empty() {
        eprint_bytes(&[b"kevy-cli: watch needs a command to run\n"]);
        return 1;
    }
    let redraw = s.opts.output == Output::Standard;
    let mut done = 0u64;
    loop {
        if redraw {
            write_out(b"\x1b[H\x1b[2J");
        }
        let heading = format!("Every {seconds}s: ");
        write_out(&[heading.as_bytes(), &command.join(&b' '), b"\n\n"].concat());
        if !s.issue(command, 1) {
            return 2;
        }
        done += 1;
        if count.is_some_and(|c| done >= c) {
            return 0;
        }
        std::thread::sleep(Duration::from_secs_f64(seconds));
    }
}

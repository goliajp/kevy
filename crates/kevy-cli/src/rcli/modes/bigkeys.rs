//! `--bigkeys` and `--memkeys`: the biggest key of each type, and the
//! average size per type, from one walk of the keyspace.

use super::pages::Pages;
use super::progress::Bar;
use super::server::{Align, allow_replica_reads, key_count, pad};
use super::sizes::{Measure, Tally, measure};
use crate::rcli::format::Output;
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};

const HEADER: &[u8] = b"\n# Scanning the entire keyspace to find biggest keys as well as\n\
# average sizes per key type.  You can use -i 0.1 to sleep 0.1 sec\n\
# per 100 SCAN commands (not usually needed).\n\n";

/// What the walk has seen.
struct Walk {
    tally: Tally,
    sampled: u64,
    name_bytes: u64,
    /// Percent done when the latest page was asked for.
    pct: f64,
    interrupted: bool,
}

/// Walk the keyspace and report; exit 0, or 1 when a step fails.
pub(crate) fn run(s: &mut Session, how: Measure) -> u8 {
    kevy_sys::install_interrupt(1);
    kevy_sys::note_interrupts();
    match walk(s, how) {
        Ok(()) => 0,
        Err(msg) => {
            eprint_bytes(&[&msg, b"\n"]);
            1
        }
    }
}

fn walk(s: &mut Session, how: Measure) -> Result<(), Vec<u8>> {
    let total = key_count(s)?;
    write_out(HEADER);
    allow_replica_reads(s)?;
    let terminal = s.opts.output == Output::Standard;
    let mut w =
        Walk { tally: Tally::new(), sampled: 0, name_bytes: 0, pct: 0.0, interrupted: false };
    let mut bar = Bar::new();
    let mut pages = Pages::new(0, s.opts.modes.pattern.clone(), s.opts.modes.count);
    let mut loops = 0u64;
    loop {
        w.pct = percent(w.sampled, total);
        let Some(keys) = pages.next(s).map_err(|e| e.text())? else { break };
        loops += 1;
        let found = measure(s, &keys, &mut w.tally, how)?;
        let lines = record(&mut w, &keys, &found, how);
        if terminal {
            bar.maybe_draw(&progress_block(&w, total, how, false));
        } else {
            write_out(&lines);
        }
        if s.opts.interval_us > 0 && loops.is_multiple_of(100) {
            std::thread::sleep(std::time::Duration::from_micros(s.opts.interval_us));
        }
        if kevy_sys::take_noted() {
            w.interrupted = true;
            break;
        }
    }
    if terminal {
        write_out(&progress_block(&w, total, how, true));
    }
    write_out(&summary(&w, how, terminal));
    Ok(())
}

/// Count a page; the progress lines a pipe gets for it.
fn record(w: &mut Walk, keys: &[Vec<u8>], found: &[Option<(usize, u64)>], how: Measure) -> Vec<u8> {
    let mut lines = Vec::new();
    for (key, hit) in keys.iter().zip(found) {
        let Some((i, size)) = *hit else { continue };
        let kind = &mut w.tally.kinds[i];
        kind.total += size;
        kind.keys += 1;
        w.name_bytes += key.len() as u64;
        w.sampled += 1;
        // Strictly bigger than the biggest so far, which starts at 0: a type
        // whose keys all measure 0 (a module type) has no biggest key.
        if kind.biggest.as_ref().map_or(0, |(big, _)| *big) < size {
            let shown = crate::rcli::repr::repr(key);
            let unit = kind.unit(how);
            lines.extend_from_slice(
                &[
                    format!("[{:05.2}%] Biggest ", w.pct).as_bytes(),
                    &pad(&kind.name, 6, Align::Left),
                    b" found so far ",
                    &shown,
                    format!(" with {size} {unit}\n").as_bytes(),
                ]
                .concat(),
            );
            kind.biggest = Some((size, shown));
        }
        if w.sampled.is_multiple_of(1_000_000) {
            lines.extend_from_slice(
                format!("[{:05.2}%] Sampled {} keys so far\n", w.pct, w.sampled).as_bytes(),
            );
        }
    }
    lines
}

/// The terminal's progress block: bar, count, one line per type. The final
/// drawing clears the type lines and returns to them, so the summary
/// overwrites them.
fn progress_block(w: &Walk, total: u64, how: Measure, last: bool) -> Vec<u8> {
    let mut out = super::progress::bar_line(percent(w.sampled, total));
    out.extend_from_slice(format!("\x1b[2K\rKeys sampled: {}\n", w.sampled).as_bytes());
    for kind in &w.tally.kinds {
        out.extend_from_slice(b"\x1b[2K\r");
        if let (false, Some((size, key))) = (last, &kind.biggest) {
            let unit = kind.unit(how);
            let line =
                [b"Biggest ".as_slice(), &pad(&kind.name, 9, Align::Left), b" found so far ", key];
            out.extend_from_slice(&line.concat());
            out.extend_from_slice(format!(" with {size} {unit}").as_bytes());
        }
        out.push(b'\n');
    }
    let up = if last { w.tally.kinds.len() } else { w.tally.kinds.len() + 2 };
    out.extend_from_slice(format!("\x1b[{up}A\r").as_bytes());
    out
}

fn summary(w: &Walk, how: Measure, terminal: bool) -> Vec<u8> {
    let mut out = b"\n-------- summary -------\n\n".to_vec();
    if !terminal {
        if w.interrupted {
            out.extend_from_slice(format!("[{:05.2}%] ", w.pct).as_bytes());
        }
        out.extend_from_slice(format!("Sampled {} keys in the keyspace!\n", w.sampled).as_bytes());
    }
    let avg = if w.name_bytes > 0 { w.name_bytes as f64 / w.sampled as f64 } else { 0.0 };
    out.extend_from_slice(
        format!("Total key length in bytes is {} (avg len {avg:.2})\n\n", w.name_bytes).as_bytes(),
    );
    for kind in &w.tally.kinds {
        if let Some((size, key)) = &kind.biggest {
            out.extend_from_slice(
                &[b"Biggest ".as_slice(), &pad(&kind.name, 6, Align::Right), b" found ", key]
                    .concat(),
            );
            out.extend_from_slice(format!(" has {size} {}\n", kind.unit(how)).as_bytes());
        }
    }
    out.push(b'\n');
    for kind in &w.tally.kinds {
        let share = if w.sampled > 0 { 100.0 * kind.keys as f64 / w.sampled as f64 } else { 0.0 };
        let avg = if kind.keys > 0 { kind.total as f64 / kind.keys as f64 } else { 0.0 };
        out.extend_from_slice(&[format!("{} ", kind.keys).as_bytes(), &kind.name].concat());
        let unit = kind.unit(how);
        out.extend_from_slice(
            format!("s with {} {unit} ({share:05.2}% of keys, avg size {avg:.2})\n", kind.total)
                .as_bytes(),
        );
    }
    out
}

fn percent(done: u64, total: u64) -> f64 {
    100.0 * done as f64 / total as f64
}

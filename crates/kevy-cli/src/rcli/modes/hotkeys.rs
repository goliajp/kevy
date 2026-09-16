//! `--hotkeys`: the keys with the highest LFU counters (`OBJECT FREQ`).

use super::pages::Pages;
use super::progress::{Bar, bar_line};
use super::server::{allow_replica_reads, key_count};
use crate::rcli::format::Output;
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};
use kevy_resp::Reply;

const HEADER: &[u8] = b"\n# Scanning the entire keyspace to find hot keys as well as\n\
# average sizes per key type.  You can use -i 0.1 to sleep 0.1 sec\n\
# per 100 SCAN commands (not usually needed).\n\n";

/// The hottest keys so far: a fixed number of slots in ascending counter
/// order, empty slots (counter 0) first. A key displaces only a strictly
/// colder one, so among equal counters the earlier key stays.
struct Hottest {
    slots: Vec<(u64, Vec<u8>)>,
}

impl Hottest {
    fn new(size: usize) -> Hottest {
        Hottest { slots: vec![(0, Vec::new()); size.max(1)] }
    }

    /// Offer a key; `true` when it took a slot.
    fn offer(&mut self, counter: u64, shown_key: Vec<u8>) -> bool {
        let above = self.slots.iter().take_while(|(c, _)| counter > *c).count();
        let Some(at) = above.checked_sub(1) else { return false };
        if at > 0 && self.slots[at].0 != 0 {
            self.slots.remove(0);
            self.slots.insert(at, (counter, shown_key));
        } else {
            self.slots[at] = (counter, shown_key);
        }
        true
    }

    /// Filled slots, hottest first, as report lines.
    fn lines(&self) -> Vec<Vec<u8>> {
        self.slots
            .iter()
            .rev()
            .filter(|(c, _)| *c > 0)
            .map(|(c, key)| {
                [format!("hot key found with counter: {c}\tkeyname: ").as_bytes(), key].concat()
            })
            .collect()
    }
}

/// Walk the keyspace and report; exit 0, or 1 when a step fails.
pub(crate) fn run(s: &mut Session) -> u8 {
    kevy_sys::install_interrupt(1);
    kevy_sys::note_interrupts();
    match walk(s) {
        Ok(()) => 0,
        Err(msg) => {
            eprint_bytes(&[&msg, b"\n"]);
            1
        }
    }
}

/// What the walk has seen.
struct Walk {
    hottest: Hottest,
    sampled: u64,
    /// Percent done when the latest page was asked for.
    pct: f64,
    interrupted: bool,
}

fn walk(s: &mut Session) -> Result<(), Vec<u8>> {
    let total = key_count(s)?;
    write_out(HEADER);
    allow_replica_reads(s)?;
    let terminal = s.opts.output == Output::Standard;
    let slots = usize::try_from(s.opts.modes.hotkeys_count).unwrap_or(0);
    let mut w = Walk { hottest: Hottest::new(slots), sampled: 0, pct: 0.0, interrupted: false };
    let mut pages = Pages::new(0, s.opts.modes.pattern.clone(), s.opts.modes.count);
    let (mut loops, mut bar) = (0u64, Bar::new());
    loop {
        w.pct = 100.0 * w.sampled as f64 / total as f64;
        let Some(keys) = pages.next(s).map_err(|e| e.text())? else { break };
        loops += 1;
        let counters = frequencies(s, &keys)?;
        let lines = record(&mut w, &keys, counters);
        if terminal {
            bar.maybe_draw(&progress_block(&w.hottest, w.sampled, total, false));
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
    report(&w, total, terminal);
    Ok(())
}

/// Count a page; the progress lines a pipe gets for it.
fn record(w: &mut Walk, keys: &[Vec<u8>], counters: Vec<u64>) -> Vec<u8> {
    let mut lines = Vec::new();
    for (key, counter) in keys.iter().zip(counters) {
        w.sampled += 1;
        if w.sampled.is_multiple_of(1_000_000) {
            lines.extend_from_slice(
                format!("[{:05.2}%] Sampled {} keys so far\n", w.pct, w.sampled).as_bytes(),
            );
        }
        let shown = crate::rcli::repr::repr(key);
        if w.hottest.offer(counter, shown.clone()) {
            lines.extend_from_slice(
                &[format!("[{:05.2}%] Hot key ", w.pct).as_bytes(), &shown].concat(),
            );
            lines.extend_from_slice(format!(" found so far with counter {counter}\n").as_bytes());
        }
    }
    lines
}

/// One OBJECT FREQ per key, pipelined; a reply that is not a counter is a
/// warning and counts as 0, an error ends the report.
fn frequencies(s: &mut Session, keys: &[Vec<u8>]) -> Result<Vec<u64>, Vec<u8>> {
    let asks: Vec<Vec<&[u8]>> = keys.iter().map(|k| vec![&b"OBJECT"[..], b"FREQ", k]).collect();
    let replies = s.pipeline(&asks).map_err(|_| b"\nI/O error".to_vec())?;
    let mut counters = Vec::with_capacity(keys.len());
    for (key, reply) in keys.iter().zip(replies) {
        counters.push(match reply {
            Reply::Int(n) => n.max(0) as u64,
            Reply::Error(msg) => return Err([b"Error: ".as_slice(), &msg].concat()),
            _ => {
                let shown = crate::rcli::repr::repr(key);
                eprint_bytes(&[
                    b"Warning: OBJECT freq on '",
                    &shown,
                    b"' failed (may have been deleted)\n",
                ]);
                0
            }
        });
    }
    Ok(counters)
}

/// Bar, count, a blank line and one line per slot; the final drawing clears
/// the slot lines and returns to them for the summary.
fn progress_block(hottest: &Hottest, sampled: u64, total: u64, last: bool) -> Vec<u8> {
    let mut out = bar_line(100.0 * sampled as f64 / total as f64);
    out.extend_from_slice(format!("\x1b[2K\rKeys sampled: {sampled}\n").as_bytes());
    let found = hottest.lines();
    let rows = hottest.slots.len() + 1;
    for row in 0..rows {
        out.extend_from_slice(b"\x1b[2K\r");
        if let (false, Some(line)) = (last, row.checked_sub(1).and_then(|i| found.get(i))) {
            out.extend_from_slice(line);
        }
        out.push(b'\n');
    }
    let up = if last { rows } else { rows + 2 };
    out.extend_from_slice(format!("\x1b[{up}A\r").as_bytes());
    out
}

/// A terminal saw the count in the progress block; a stopped walk says how
/// far it got.
fn report(w: &Walk, total: u64, terminal: bool) {
    let mut out = Vec::new();
    if terminal {
        out.extend_from_slice(&progress_block(&w.hottest, w.sampled, total, true));
    }
    out.extend_from_slice(b"\n-------- summary -------\n\n");
    if !terminal {
        if w.interrupted {
            out.extend_from_slice(format!("[{:05.2}%] ", w.pct).as_bytes());
        }
        out.extend_from_slice(format!("Sampled {} keys in the keyspace!\n", w.sampled).as_bytes());
    }
    for line in w.hottest.lines() {
        out.extend_from_slice(&line);
        out.push(b'\n');
    }
    write_out(&out);
}

#[cfg(test)]
mod tests {
    use super::Hottest;

    fn keys(h: &Hottest) -> Vec<String> {
        h.lines().iter().map(|l| String::from_utf8_lossy(l).into_owned()).collect()
    }

    #[test]
    fn equal_counters_keep_the_earlier_key_and_a_full_pool_drops_the_coldest() {
        let mut h = Hottest::new(2);
        assert!(h.offer(6, b"a".to_vec()));
        assert!(h.offer(5, b"d".to_vec()));
        assert!(!h.offer(5, b"b".to_vec()), "not hotter than the coldest kept");
        assert!(!h.offer(0, b"z".to_vec()), "a zero counter is never hot");
        assert!(h.offer(6, b"c".to_vec()), "hotter than d");
        assert_eq!(
            keys(&h),
            [
                "hot key found with counter: 6\tkeyname: a",
                "hot key found with counter: 6\tkeyname: c"
            ]
        );
    }
}

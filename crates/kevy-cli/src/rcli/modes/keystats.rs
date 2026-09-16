//! `--keystats`: sizes and lengths across the keyspace — the biggest keys,
//! per-type tops, a size distribution, key name lengths, and a type table.

use super::hdr::Histogram;
use super::pages::Pages;
use super::progress::Bar;
use super::server::{allow_replica_reads, key_count};
use super::sizes::{Measure, Tally, measure};
use crate::rcli::format::Output;
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};

const HEADER: &[u8] =
    b"\n# Scanning the entire keyspace to find the biggest keys and distribution information.\n\
# Use -i 0.1 to sleep 0.1 sec per 100 SCAN commands (not usually needed).\n\
# Use --cursor <n> to start the scan at the cursor <n> (usually after a Ctrl-C).\n\
# Use --top <n> to display <n> top key sizes (default is 10).\n\
# Ctrl-C to stop the scan.\n\n";

/// The upper bounds of the key name length buckets.
pub(crate) const NAME_BUCKETS: [u64; 8] =
    [32, 256, 64 << 10, 1 << 20, 16 << 20, 128 << 20, 512 << 20, u64::MAX];

/// A type's figures: memory and length, and the key with the most of each.
#[derive(Debug, Default)]
pub(crate) struct TypeStats {
    pub(crate) keys: u64,
    pub(crate) memory: u64,
    pub(crate) length: u64,
    pub(crate) biggest_memory: Option<(u64, Vec<u8>)>,
    pub(crate) biggest_length: Option<(u64, Vec<u8>)>,
}

/// Everything the report is made of.
pub(crate) struct Stats {
    pub(crate) total: u64,
    pub(crate) sampled: u64,
    pub(crate) memory: u64,
    pub(crate) sizes: Histogram,
    pub(crate) name_bytes: u64,
    pub(crate) longest_name: u64,
    pub(crate) names: [u64; NAME_BUCKETS.len()],
    /// The biggest keys by memory: (size, kind, quoted key), biggest first.
    pub(crate) top: Vec<(u64, usize, Vec<u8>)>,
    pub(crate) top_wanted: usize,
    pub(crate) kinds: Tally,
    /// Indexed like `kinds.kinds`.
    pub(crate) per_kind: Vec<TypeStats>,
    /// Set when Ctrl-C stopped the walk: where to resume.
    pub(crate) resume_at: Option<u64>,
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

fn walk(s: &mut Session) -> Result<(), Vec<u8>> {
    let total = key_count(s)?;
    write_out(HEADER);
    allow_replica_reads(s)?;
    let terminal = s.opts.output == Output::Standard;
    let samples = s.opts.modes.memkeys_samples;
    let mut st = Stats::new(total, usize::try_from(s.opts.modes.top).unwrap_or(usize::MAX));
    let mut pages =
        Pages::new(s.opts.modes.cursor, s.opts.modes.pattern.clone(), s.opts.modes.count);
    let (mut loops, mut bar) = (0u64, Bar::new());
    while let Some(keys) = pages.next(s).map_err(|e| e.text())? {
        loops += 1;
        let measured =
            measure(s, &keys, &mut st.kinds, &[Measure::Memory { samples }, Measure::Length])?;
        for (key, hit) in keys.iter().zip(measured) {
            if let Some((kind, sizes)) = hit {
                st.count(key, kind, sizes[0], sizes[1]);
            }
        }
        if terminal {
            bar.maybe_draw(&super::keystats_report::screen(&st, false));
        }
        if s.opts.interval_us > 0 && loops.is_multiple_of(100) {
            std::thread::sleep(std::time::Duration::from_micros(s.opts.interval_us));
        }
        if kevy_sys::take_noted() {
            st.resume_at = Some(pages.cursor());
            break;
        }
    }
    let report = if terminal {
        super::keystats_report::screen(&st, true)
    } else {
        super::keystats_report::text(&st)
    };
    write_out(&report);
    Ok(())
}

impl Stats {
    fn new(total: u64, top_wanted: usize) -> Stats {
        Stats {
            total,
            sampled: 0,
            memory: 0,
            sizes: Histogram::default(),
            name_bytes: 0,
            longest_name: 0,
            names: [0; NAME_BUCKETS.len()],
            top: Vec::new(),
            top_wanted,
            kinds: Tally::new(),
            per_kind: Vec::new(),
            resume_at: None,
        }
    }

    fn count(&mut self, key: &[u8], kind: usize, memory: u64, length: u64) {
        self.sampled += 1;
        self.memory += memory;
        self.sizes.record(memory);
        let name = key.len() as u64;
        self.name_bytes += name;
        self.longest_name = self.longest_name.max(name);
        let bucket = NAME_BUCKETS.iter().position(|&b| name <= b).unwrap_or(NAME_BUCKETS.len() - 1);
        self.names[bucket] += 1;
        if self.per_kind.len() < self.kinds.kinds.len() {
            self.per_kind.resize_with(self.kinds.kinds.len(), TypeStats::default);
        }
        let shown = crate::rcli::repr::repr(key);
        let t = &mut self.per_kind[kind];
        t.keys += 1;
        t.memory += memory;
        t.length += length;
        if t.biggest_memory.as_ref().map_or(0, |(m, _)| *m) < memory {
            t.biggest_memory = Some((memory, shown.clone()));
        }
        if t.biggest_length.as_ref().map_or(0, |(l, _)| *l) < length {
            t.biggest_length = Some((length, shown.clone()));
        }
        // Biggest first; a key joins after the keys of its own size.
        let at = self.top.iter().position(|(m, _, _)| *m < memory).unwrap_or(self.top.len());
        if at < self.top_wanted {
            self.top.insert(at, (memory, kind, shown));
            self.top.truncate(self.top_wanted);
        }
    }
}

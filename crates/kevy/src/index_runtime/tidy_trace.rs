//! Timing each index repack step and tick, for the harness build only.
//!
//! Every step lands in a log-bucketed histogram per shard thread, split
//! by what happened during it: whether it freed a leaf, whether the
//! thread took a page fault, whether it lost the CPU. Every tick lands in
//! a second histogram. A step at or over `KEVY_REPACK_TRACE_US`
//! microseconds (100 by default) is also kept whole — what the step did
//! to the tree and what the thread went through — and the slowest 4096
//! survive. A background thread rewrites the file `KEVY_REPACK_TRACE`
//! names once a second; without it nothing is written.
//!
//! The thread counters cost two system calls on each side of a step;
//! they are read outside the timed span, so a step's time is the pack
//! alone, but the tick carrying them runs longer than the product's.

use std::cell::Cell;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use kevy_index::{Segment, TidyProbe};
use kevy_sys::ThreadUsage;

use crate::index_runtime::ShardIndexes;

/// Shard threads with a histogram of their own; any beyond share the last.
const SLOTS: usize = 16;
/// Four buckets an octave of nanoseconds, up to about 2^40 ns.
const BUCKETS: usize = 164;
/// Step classes: bit 0 freed a leaf, bit 1 faulted, bit 2 lost the CPU.
const CLASSES: usize = 8;
const KEEP: usize = 4096;

struct Hist([AtomicU64; BUCKETS]);

impl Hist {
    const fn new() -> Hist {
        Hist([const { AtomicU64::new(0) }; BUCKETS])
    }
}

struct Slot {
    steps: [Hist; CLASSES],
    ticks: Hist,
    step_ns: AtomicU64,
    tick_ns: AtomicU64,
    leaves_freed: AtomicU64,
    entries_moved: AtomicU64,
}

static SLOT_ARRAY: [Slot; SLOTS] = [const {
    Slot {
        steps: [const { Hist::new() }; CLASSES],
        ticks: Hist::new(),
        step_ns: AtomicU64::new(0),
        tick_ns: AtomicU64::new(0),
        leaves_freed: AtomicU64::new(0),
        entries_moved: AtomicU64::new(0),
    }
}; SLOTS];
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static MY_SLOT: Cell<usize> = const { Cell::new(usize::MAX) };
    /// When this thread's current tick started, and its steps so far.
    static TICK: Cell<(Option<Instant>, u32)> = const { Cell::new((None, 0)) };
}

/// One step slower than the threshold, with its context.
#[derive(Clone, Copy, Debug)]
struct Long {
    slot: usize,
    at: Duration,
    wall_ns: u64,
    in_tick_ns: u64,
    nth_in_tick: u32,
    more: bool,
    use0: ThreadUsage,
    use1: ThreadUsage,
    p0: TidyProbe,
    p1: TidyProbe,
}

struct Config {
    long_ns: u64,
    started: Instant,
    kept: Mutex<(Vec<Long>, u64)>,
}

fn config() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let us = std::env::var("KEVY_REPACK_TRACE_US").ok().and_then(|v| v.parse().ok());
        let cfg = Config {
            long_ns: us.unwrap_or(100u64) * 1000,
            started: Instant::now(),
            kept: Mutex::new((Vec::with_capacity(KEEP), 0)),
        };
        if let Some(path) = std::env::var_os("KEVY_REPACK_TRACE") {
            let spawned = std::thread::Builder::new()
                .name("repack-trace".into())
                .spawn(move || dump_forever(std::path::PathBuf::from(path)));
            if let Err(e) = spawned {
                eprintln!("repack trace: no writer thread: {e}");
            }
        }
        cfg
    })
}

fn slot() -> &'static Slot {
    &SLOT_ARRAY[slot_index()]
}

fn slot_index() -> usize {
    MY_SLOT.with(|c| {
        if c.get() == usize::MAX {
            c.set(NEXT_SLOT.fetch_add(1, Relaxed).min(SLOTS - 1));
        }
        c.get()
    })
}

fn bucket(ns: u64) -> usize {
    if ns < 4 {
        return ns as usize;
    }
    let e = 63 - ns.leading_zeros() as usize;
    (e * 4 + ((ns >> (e - 2)) & 3) as usize).min(BUCKETS - 1)
}

fn bucket_floor(b: usize) -> u64 {
    if b < 8 { b as u64 } else { (4 + (b % 4) as u64) << (b / 4 - 2) }
}

fn usage() -> ThreadUsage {
    kevy_sys::thread_usage().unwrap_or_default()
}

/// The tick, timed whole.
pub(in crate::index_runtime) fn run(st: &mut ShardIndexes) {
    config();
    let t0 = Instant::now();
    TICK.with(|c| c.set((Some(t0), 0)));
    super::tick(st);
    let ns = t0.elapsed().as_nanos() as u64;
    TICK.with(|c| c.set((None, 0)));
    let s = slot();
    s.ticks.0[bucket(ns)].fetch_add(1, Relaxed);
    s.tick_ns.fetch_add(ns, Relaxed);
}

/// One segment's step, timed, classed and, when slow, kept.
pub(super) fn step(seg: &mut Segment) -> bool {
    let cfg = config();
    let (use0, p0) = (usage(), seg.tidy_probe());
    let t0 = Instant::now();
    let more = super::pack(seg);
    let wall_ns = t0.elapsed().as_nanos() as u64;
    let (use1, p1) = (usage(), seg.tidy_probe());
    let (tick_t0, nth) = TICK.with(|c| {
        let (t, n) = c.get();
        c.set((t, n + 1));
        (t, n)
    });
    let freed = p0.leaves.saturating_sub(p1.leaves);
    let faulted = use1.minor_faults + use1.major_faults > use0.minor_faults + use0.major_faults;
    let switched = use1.voluntary_switches + use1.involuntary_switches
        > use0.voluntary_switches + use0.involuntary_switches;
    let class = usize::from(freed > 0) | usize::from(faulted) << 1 | usize::from(switched) << 2;
    let s = slot();
    s.steps[class].0[bucket(wall_ns)].fetch_add(1, Relaxed);
    s.step_ns.fetch_add(wall_ns, Relaxed);
    s.leaves_freed.fetch_add(freed as u64, Relaxed);
    s.entries_moved.fetch_add(p1.lap_moved.saturating_sub(p0.lap_moved) as u64, Relaxed);
    if wall_ns >= cfg.long_ns {
        let in_tick_ns = tick_t0.map_or(0, |t| t0.saturating_duration_since(t).as_nanos() as u64);
        keep(
            cfg,
            Long {
                slot: slot_index(),
                at: t0.saturating_duration_since(cfg.started),
                wall_ns,
                in_tick_ns,
                nth_in_tick: nth,
                more,
                use0,
                use1,
                p0,
                p1,
            },
        );
    }
    more
}

/// Keep the slowest [`KEEP`] steps: once full, a new one replaces the
/// fastest kept if it is slower. A shard never waits for the writer
/// thread: a step that finds the list busy is only counted.
fn keep(cfg: &Config, rec: Long) {
    let Ok(mut kept) = cfg.kept.try_lock() else {
        DROPPED.fetch_add(1, Relaxed);
        return;
    };
    kept.1 += 1;
    if kept.0.len() < KEEP {
        kept.0.push(rec);
    } else if let Some(min) = kept.0.iter_mut().min_by_key(|r| r.wall_ns)
        && min.wall_ns < rec.wall_ns
    {
        *min = rec;
    }
}

fn dump_forever(path: std::path::PathBuf) {
    let tmp = path.with_extension("tmp");
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let text = render(config());
        if let Err(e) = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, &path)) {
            eprintln!("repack trace: {}: {e}", path.display());
        }
    }
}

fn render(cfg: &Config) -> String {
    let mut out = String::new();
    let used = NEXT_SLOT.load(Relaxed).min(SLOTS);
    let _ = writeln!(
        out,
        "trace uptime_ms={} long_ns={} step_leaves={} slots={used}",
        cfg.started.elapsed().as_millis(),
        cfg.long_ns,
        super::STEP_LEAVES,
    );
    for (i, s) in SLOT_ARRAY.iter().enumerate().take(used) {
        let _ = writeln!(
            out,
            "slot {i} step_ns={} tick_ns={} leaves_freed={} entries_moved={}",
            s.step_ns.load(Relaxed),
            s.tick_ns.load(Relaxed),
            s.leaves_freed.load(Relaxed),
            s.entries_moved.load(Relaxed),
        );
        for (class, h) in s.steps.iter().enumerate() {
            hist_line(&mut out, &format!("hist {i} step {class}"), h);
        }
        hist_line(&mut out, &format!("hist {i} tick -"), &s.ticks);
    }
    let (kept, total) = {
        let k = cfg.kept.lock().unwrap_or_else(PoisonError::into_inner);
        (k.0.clone(), k.1)
    };
    let _ = writeln!(out, "long_total {total} not_kept_busy {}", DROPPED.load(Relaxed));
    for r in &kept {
        long_line(&mut out, r);
    }
    out
}

fn hist_line(out: &mut String, head: &str, h: &Hist) {
    let mut line = String::from(head);
    for (b, c) in h.0.iter().enumerate() {
        let c = c.load(Relaxed);
        if c > 0 {
            let _ = write!(line, " {}:{c}", bucket_floor(b));
        }
    }
    let _ = writeln!(out, "{line}");
}

fn long_line(out: &mut String, r: &Long) {
    let d = |a: u64, b: u64| b.saturating_sub(a);
    let (u0, u1, p0, p1) = (&r.use0, &r.use1, &r.p0, &r.p1);
    let _ = writeln!(
        out,
        "long slot={} at_ms={} wall_ns={} cpu_ns={} minflt={} majflt={} nvcsw={} nivcsw={} \
         in_tick_ns={} nth_in_tick={} more={} entries={} leaves={} leaves_freed={} \
         inners_freed={} height={}->{} moved={} lap_moved={}->{} sep_bytes={}->{} \
         overflow_bytes={}->{} free_list_cap={}->{} hand_cap={}->{} resting={}->{}",
        r.slot,
        r.at.as_millis(),
        r.wall_ns,
        d(u0.cpu_ns, u1.cpu_ns),
        d(u0.minor_faults, u1.minor_faults),
        d(u0.major_faults, u1.major_faults),
        d(u0.voluntary_switches, u1.voluntary_switches),
        d(u0.involuntary_switches, u1.involuntary_switches),
        r.in_tick_ns,
        r.nth_in_tick,
        u8::from(r.more),
        p0.entries,
        p0.leaves,
        p0.leaves.saturating_sub(p1.leaves),
        p0.inners.saturating_sub(p1.inners),
        p0.height,
        p1.height,
        p1.lap_moved.saturating_sub(p0.lap_moved),
        p0.lap_moved,
        p1.lap_moved,
        p0.sep_bytes,
        p1.sep_bytes,
        p0.overflow_bytes,
        p1.overflow_bytes,
        p0.free_list_cap,
        p1.free_list_cap,
        p0.hand_cap,
        p1.hand_cap,
        u8::from(p0.resting),
        u8::from(p1.resting),
    );
}

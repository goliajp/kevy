//! Observability slots owned by [`RuntimeState`]: per-shard stats,
//! the ops-per-sec sampler ring, and the audit-log handle.
//!
//! [`RuntimeState`]: crate::RuntimeState

// Best effort. What matters is reported by the path that owns the
// outcome — the next read, the next tick, the returned value — and
// this call is the notification, not the result.
#![expect(
    clippy::let_underscore_must_use,
    reason = "best effort, with the real outcome reported elsewhere"
)]

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// One shard's observability slot. All atomics are `Relaxed`: these are
/// statistics, never used to establish happens-before.
#[derive(Debug, Default)]
pub(crate) struct ShardStats {
    pub used_memory: AtomicU64,
    pub used_memory_peak: AtomicU64,
    pub keys: AtomicU64,
    pub expires: AtomicU64,
    pub expired_keys: AtomicU64,
    pub evicted_keys: AtomicU64,
    pub commands_processed: AtomicU64,
    pub connections_received: AtomicU64,
    /// Live client conns on this shard right now (gauge, published per
    /// tick from the reactor's conn table; cluster-bus links excluded).
    pub clients_connected: AtomicU64,
    /// Client conns parked in a blocking command on this shard right now
    /// (gauge, published per tick alongside `clients_connected`).
    pub blocked_clients: AtomicU64,
    /// High-water mark of the reactor tick's lateness (µs over its
    /// interval) — the single-iteration stall upper bound (V3 tail
    /// train). fetch_max'd from the tick, never reset.
    pub tick_gap_max_us: AtomicU64,
    /// Connections closed for crossing the query-buffer cap. Redis
    /// calls this `client_query_buffer_limit_disconnections`; it exists
    /// here because an intermittent "the server did not close" could
    /// not be told from "it decided and the close had not landed".
    pub query_buffer_disconnections: AtomicU64,
    /// Reactor tick bodies run on this shard since boot. The gap gauge
    /// above is a high-water mark and cannot answer "how OFTEN does
    /// housekeeping run" — a single 400 ms outlier and a chronically
    /// starved 2 Hz cadence produce the same number. Two reads of this
    /// counter divided by the elapsed time answer it directly.
    pub ticks_total: AtomicU64,
    /// This shard's tiering gauges (all zero when
    /// tiering is off — `tier_enabled` is the section gate).
    pub tier: TierGauges,
    /// This shard's allocator terms (all zero, `reporting` included,
    /// when kevy-alloc is not the global allocator).
    pub alloc: AllocGauges,
}

/// One shard's `INFO # Tiering` slot — mirrors
/// `kevy_store::TierStats`, published per tick alongside the memory
/// gauges. All `Relaxed` (statistics, like everything else here).
#[derive(Debug, Default)]
pub(crate) struct TierGauges {
    /// 1 when this shard's store has tiering enabled.
    pub enabled: AtomicU64,
    pub budget: AtomicU64,
    pub effective_target: AtomicU64,
    pub reserved_bytes: AtomicU64,
    pub stub_bytes: AtomicU64,
    pub cold_keys: AtomicU64,
    pub cold_bytes: AtomicU64,
    pub demotions_total: AtomicU64,
    pub promotions_total: AtomicU64,
    pub peek_preads_total: AtomicU64,
    pub batch_submissions_total: AtomicU64,
    pub vlog_files: AtomicU64,
    pub vlog_bytes: AtomicU64,
    pub vlog_live_bytes: AtomicU64,
    pub vlog_epoch: AtomicU64,
}

/// One shard's `INFO # Allocator` slot — mirrors `kevy_alloc::Stats`,
/// published per tick alongside the memory gauges by the shard thread
/// that owns the heap being described (`thread_stats` answers for the
/// calling thread only, so this is the one place it can be read).
///
/// The section exists because the accounting identity — every mapped
/// byte in exactly one bucket — is the only way to say WHERE a resident
/// ratio went. Without it, the one workload where this allocator loses
/// to glibc can be measured but not explained.
#[derive(Debug, Default)]
pub(crate) struct AllocGauges {
    /// 1 once this shard has published a real snapshot. The section
    /// gate: a zeroed slot and a heap that genuinely holds nothing
    /// are not the same answer.
    pub reporting: AtomicU64,
    pub mapped: AtomicU64,
    pub live: AtomicU64,
    pub rounding: AtomicU64,
    pub cache: AtomicU64,
    pub span_free: AtomicU64,
    pub returned: AtomicU64,
    pub virgin: AtomicU64,
    pub hysteresis: AtomicU64,
    pub segment_overhead: AtomicU64,
    pub large_count: AtomicU64,
    pub spans_assigned: AtomicU64,
}

/// Process-wide totals, summed across every shard slot.
#[derive(Default)]
pub(crate) struct Totals {
    pub used_memory: u64,
    pub used_memory_peak: u64,
    pub keys: u64,
    pub expires: u64,
    pub expired_keys: u64,
    pub evicted_keys: u64,
    pub commands_processed: u64,
    pub connections_received: u64,
    pub clients_connected: u64,
    /// SUM across shards — each blocked conn is registered on exactly one.
    pub blocked_clients: u64,
    /// MAX across shards (a stall on one shard is the instance's
    /// answer — summing stalls would say something false).
    pub tick_gap_max_us: u64,
    /// SUM across shards — each shard closes its own connections.
    pub query_buffer_disconnections: u64,
    /// SUM across shards — every shard ticks on its own clock, so the
    /// instance's housekeeping rate is the total (divide by shard count
    /// for the per-shard cadence).
    pub ticks_total: u64,
    /// Tiering totals. `tier_enabled` = any shard tiers (they
    /// all do or none does — the config is process-wide).
    pub tier_enabled: bool,
    pub tier: TierTotals,
    /// How many shards published an allocator snapshot. 0 = the section
    /// is absent; it is also the denominator for reading the sums.
    pub alloc_shards: u64,
    pub alloc: AllocTotals,
}

/// The summed `# Tiering` gauges (budgets, floors and vlog gauges are
/// per-shard slices; their sums are the process-level view INFO shows).
#[derive(Default)]
pub(crate) struct TierTotals {
    pub budget: u64,
    pub effective_target: u64,
    pub reserved_bytes: u64,
    pub stub_bytes: u64,
    pub cold_keys: u64,
    pub cold_bytes: u64,
    pub demotions_total: u64,
    pub promotions_total: u64,
    pub peek_preads_total: u64,
    pub batch_submissions_total: u64,
    pub vlog_files: u64,
    pub vlog_bytes: u64,
    pub vlog_live_bytes: u64,
    pub vlog_epoch: u64,
}

/// The summed allocator terms. Each shard has its own heap and the
/// buckets are disjoint within one, so the sums stay an identity:
/// `accounted` over all shards is comparable to `mapped` over all
/// shards exactly as it is per-heap.
#[derive(Default)]
pub(crate) struct AllocTotals {
    pub mapped: u64,
    pub live: u64,
    pub rounding: u64,
    pub cache: u64,
    pub span_free: u64,
    pub returned: u64,
    pub virgin: u64,
    pub hysteresis: u64,
    pub segment_overhead: u64,
    pub large_count: u64,
    pub spans_assigned: u64,
}

impl AllocTotals {
    /// Fold one shard's heap into the totals. Every bucket is disjoint
    /// within a heap and the heaps are disjoint from each other, so the
    /// identity survives the sum term by term.
    fn add(&mut self, g: &AllocGauges) {
        self.mapped += g.mapped.load(Relaxed);
        self.live += g.live.load(Relaxed);
        self.rounding += g.rounding.load(Relaxed);
        self.cache += g.cache.load(Relaxed);
        self.span_free += g.span_free.load(Relaxed);
        self.returned += g.returned.load(Relaxed);
        self.virgin += g.virgin.load(Relaxed);
        self.hysteresis += g.hysteresis.load(Relaxed);
        self.segment_overhead += g.segment_overhead.load(Relaxed);
        self.large_count += g.large_count.load(Relaxed);
        self.spans_assigned += g.spans_assigned.load(Relaxed);
    }

    /// The sum the identity asserts, mirroring `Stats::accounted` — kept
    /// separate from [`Self::mapped`] so a reader compares the two
    /// rather than being handed a difference someone else computed.
    pub fn accounted(&self) -> u64 {
        self.live
            + self.rounding
            + self.cache
            + self.span_free
            + self.returned
            + self.virgin
            + self.hysteresis
            + self.segment_overhead
    }
}

/// Retained ops-per-sec samples — 16 × 100 ms default tick ≈ a 1.6 s window.
const OPS_WINDOW: usize = 16;

#[derive(Debug)]
pub(crate) struct ObsState {
    /// Append-only ADMIN-command audit log. `None` = OFF (`[audit]
    /// log_path` empty, or the file failed to open at boot).
    audit: Option<Mutex<File>>,
    /// One slot per shard, preallocated at construction so the hot
    /// path never takes a registry lock. Gauges are overwritten per
    /// tick; counters are published from the shard thread-locals.
    shard_stats: Box<[Arc<ShardStats>]>,
    /// `(elapsed_ms_since_start, total_commands_processed)` samples,
    /// pushed by the lead shard once per tick.
    ops_ring: Mutex<Vec<(u128, u64)>>,
    /// One replication-view slot per shard, overwritten each tick by
    /// `Commands::on_replication_view`. INFO replication / ROLE answer
    /// on one shard but must report the whole instance — they fold
    /// every slot (offset sum + per-replica row union).
    repl_views: Box<[Mutex<ReplShardView>]>,
    /// Anchor for the monotonic millisecond clock the sampler uses.
    start: Instant,
}

/// One shard's per-tick replication view: its `master_repl_offset`
/// plus a row per handshake-complete replica conn.
#[derive(Debug, Clone, Default)]
pub(crate) struct ReplShardView {
    pub(crate) offset: u64,
    pub(crate) replicas: Vec<kevy_rt::ReplicaViewRow>,
}

impl ObsState {
    pub(crate) fn new(audit_log_path: &Path, nshards: usize) -> Self {
        Self {
            audit: open_audit_log(audit_log_path),
            shard_stats: (0..nshards.max(1)).map(|_| Arc::new(ShardStats::default())).collect(),
            ops_ring: Mutex::new(Vec::new()),
            repl_views: (0..nshards.max(1)).map(|_| Mutex::new(ReplShardView::default())).collect(),
            start: Instant::now(),
        }
    }

    /// Overwrite shard `shard`'s replication-view slot (per-tick
    /// publication; an out-of-range shard is ignored).
    pub(crate) fn publish_repl_view(&self, shard: usize, view: ReplShardView) {
        if let Some(slot) = self.repl_views.get(shard) {
            *slot.lock().expect("repl_views poisoned") = view;
        }
    }

    /// Snapshot every shard's replication view for aggregation.
    pub(crate) fn repl_views(&self) -> Vec<ReplShardView> {
        self.repl_views.iter().map(|s| s.lock().expect("repl_views poisoned").clone()).collect()
    }

    /// Shard `shard`'s stats slot, for the thread-local cache set up by
    /// `Commands::on_shard_start`. `None` when the runtime was built
    /// with more shards than this state was sized for.
    pub(crate) fn slot(&self, shard: usize) -> Option<Arc<ShardStats>> {
        self.shard_stats.get(shard).cloned()
    }

    /// Sum every shard's slot for the process-wide `INFO` view.
    pub(crate) fn aggregate(&self) -> Totals {
        let mut t = Totals::default();
        for s in &self.shard_stats {
            t.used_memory += s.used_memory.load(Relaxed);
            t.used_memory_peak += s.used_memory_peak.load(Relaxed);
            t.keys += s.keys.load(Relaxed);
            t.expires += s.expires.load(Relaxed);
            t.expired_keys += s.expired_keys.load(Relaxed);
            t.evicted_keys += s.evicted_keys.load(Relaxed);
            t.commands_processed += s.commands_processed.load(Relaxed);
            t.connections_received += s.connections_received.load(Relaxed);
            t.clients_connected += s.clients_connected.load(Relaxed);
            t.blocked_clients += s.blocked_clients.load(Relaxed);
            t.tick_gap_max_us = t.tick_gap_max_us.max(s.tick_gap_max_us.load(Relaxed));
            t.ticks_total += s.ticks_total.load(Relaxed);
            t.query_buffer_disconnections += s.query_buffer_disconnections.load(Relaxed);
            t.tier_enabled |= s.tier.enabled.load(Relaxed) != 0;
            t.tier.budget += s.tier.budget.load(Relaxed);
            t.tier.effective_target += s.tier.effective_target.load(Relaxed);
            t.tier.reserved_bytes += s.tier.reserved_bytes.load(Relaxed);
            t.tier.stub_bytes += s.tier.stub_bytes.load(Relaxed);
            t.tier.cold_keys += s.tier.cold_keys.load(Relaxed);
            t.tier.cold_bytes += s.tier.cold_bytes.load(Relaxed);
            t.tier.demotions_total += s.tier.demotions_total.load(Relaxed);
            t.tier.promotions_total += s.tier.promotions_total.load(Relaxed);
            t.tier.peek_preads_total += s.tier.peek_preads_total.load(Relaxed);
            t.tier.batch_submissions_total += s.tier.batch_submissions_total.load(Relaxed);
            t.tier.vlog_files += s.tier.vlog_files.load(Relaxed);
            t.tier.vlog_bytes += s.tier.vlog_bytes.load(Relaxed);
            t.tier.vlog_live_bytes += s.tier.vlog_live_bytes.load(Relaxed);
            t.tier.vlog_epoch += s.tier.vlog_epoch.load(Relaxed);
            if s.alloc.reporting.load(Relaxed) != 0 {
                t.alloc_shards += 1;
                t.alloc.add(&s.alloc);
            }
        }
        t
    }

    /// Push one ops-per-sec sample. Called once per tick by the lead
    /// shard (see `ops::stats::sample_ops_if_lead`).
    pub(crate) fn push_ops_sample(&self, total_commands: u64) {
        let mut ring = self.ops_ring.lock().expect("ops_ring poisoned");
        ring.push((self.elapsed_ms(), total_commands));
        if ring.len() > OPS_WINDOW {
            let drop = ring.len() - OPS_WINDOW;
            ring.drain(0..drop);
        }
    }

    /// Average commands/sec over the retained sample window. `current`
    /// is the live process-wide command total (so the most recent
    /// traffic counts even between samples). Returns 0 until two
    /// samples span a non-zero interval.
    pub(crate) fn instantaneous_ops_per_sec(&self, current: u64) -> u64 {
        let ring = self.ops_ring.lock().expect("ops_ring poisoned");
        let Some(&(oldest_ms, oldest_cmds)) = ring.first() else {
            return 0;
        };
        let dt_ms = self.elapsed_ms().saturating_sub(oldest_ms);
        if dt_ms == 0 {
            return 0;
        }
        let dc = current.saturating_sub(oldest_cmds);
        ((u128::from(dc) * 1000) / dt_ms) as u64
    }

    fn elapsed_ms(&self) -> u128 {
        self.start.elapsed().as_millis()
    }

    /// Log one ADMIN command event. `args` includes the verb. Best-effort
    /// write — audit write failures never abort the calling command path.
    /// Format: `<unix_micros>\t<command>\t<arg1>\t...\n`, one line per
    /// event, args truncated at 256 bytes and tab/newline-sanitised.
    pub(crate) fn audit_record(&self, args: &[&[u8]]) {
        let Some(mu) = &self.audit else { return };
        let mut line = String::with_capacity(128);
        let micros =
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0);
        line.push_str(&micros.to_string());
        for arg in args {
            line.push('\t');
            let s = String::from_utf8_lossy(&arg[..arg.len().min(256)]);
            for c in s.chars() {
                match c {
                    '\t' | '\n' | '\r' => line.push(' '),
                    _ => line.push(c),
                }
            }
            if arg.len() > 256 {
                line.push('…');
            }
        }
        line.push('\n');
        if let Ok(mut f) = mu.lock() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }
}

/// Open the audit log in append-only mode (`O_APPEND`; process death
/// never corrupts). Empty path = audit OFF; an open failure is loud on
/// stderr but non-fatal (the server still boots, audit stays OFF).
fn open_audit_log(path: &Path) -> Option<Mutex<File>> {
    if path.as_os_str().is_empty() {
        return None;
    }
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => Some(Mutex::new(f)),
        Err(e) => {
            eprintln!("kevy: audit log {} could not open: {e}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn audit_off_when_path_empty() {
        let obs = ObsState::new(Path::new(""), 1);
        // Must be a silent no-op.
        obs.audit_record(&[b"CONFIG", b"SET", b"maxmemory", b"1g"]);
        assert!(obs.audit.is_none());
    }

    #[test]
    fn audit_records_one_sanitised_line() {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path: PathBuf = std::env::temp_dir().join(format!("kevy-audit-{nanos}"));
        let obs = ObsState::new(&path, 1);
        obs.audit_record(&[b"DEBUG", b"tab\there"]);
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("DEBUG\ttab here"), "got: {text:?}");
    }

    #[test]
    fn aggregate_sums_across_slots() {
        let obs = ObsState::new(Path::new(""), 3);
        obs.shard_stats[0].keys.store(2, Relaxed);
        obs.shard_stats[2].keys.store(5, Relaxed);
        assert_eq!(obs.aggregate().keys, 7);
    }

    #[test]
    fn ops_ring_caps_at_window() {
        let obs = ObsState::new(Path::new(""), 1);
        for i in 0..(OPS_WINDOW as u64 + 10) {
            obs.push_ops_sample(i);
        }
        assert_eq!(obs.ops_ring.lock().unwrap().len(), OPS_WINDOW);
    }

    /// The allocator fold, and its gate. The publisher is behind the
    /// `kevy-alloc` feature and the fold is not, so with the feature off
    /// — which is the default build, and how the coverage corpus runs —
    /// nothing else here ever executes this path.
    ///
    /// The gate is the part worth asserting: a slot that never reported
    /// holds nine zeroes, and nine zeroes are also a legitimate reading
    /// of a heap. Counting the first as the second is what would let
    /// INFO name an allocator that is not running.
    #[test]
    fn only_shards_that_reported_are_folded_in() {
        let obs = ObsState::new(Path::new(""), 3);
        let a = obs.slot(0).expect("slot 0");
        a.alloc.mapped.store(8_388_608, Relaxed);
        a.alloc.live.store(700_000, Relaxed);
        a.alloc.hysteresis.store(7_688_608, Relaxed);
        a.alloc.reporting.store(1, Relaxed);

        let b = obs.slot(1).expect("slot 1");
        b.alloc.mapped.store(4_194_304, Relaxed);
        b.alloc.live.store(100_000, Relaxed);
        b.alloc.hysteresis.store(4_094_304, Relaxed);
        b.alloc.reporting.store(1, Relaxed);

        // Shard 2 never published. Its zeroes must not be read as a
        // third heap that happens to hold nothing.
        let t = obs.aggregate();
        assert_eq!(t.alloc_shards, 2, "a silent shard was counted as a reporting one");
        assert_eq!(t.alloc.mapped, 12_582_912);
        assert_eq!(t.alloc.live, 800_000);
        // Disjoint within a heap and between heaps, so the identity
        // survives the sum: this is the property the section rests on.
        assert_eq!(t.alloc.accounted(), t.alloc.mapped, "the summed terms stopped partitioning");
    }
}

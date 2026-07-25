//! Observability slots owned by [`RuntimeState`]: per-shard stats,
//! the ops-per-sec sampler ring, and the audit-log handle.
//!
//! [`RuntimeState`]: crate::RuntimeState

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// One shard's observability slot. All atomics are `Relaxed`: these are
/// statistics, never used to establish happens-before.
#[derive(Default)]
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
    /// This shard's tiering gauges (all zero when
    /// tiering is off — `tier_enabled` is the section gate).
    pub tier: TierGauges,
}

/// One shard's `INFO # Tiering` slot — mirrors
/// `kevy_store::TierStats`, published per tick alongside the memory
/// gauges. All `Relaxed` (statistics, like everything else here).
#[derive(Default)]
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
    /// Tiering totals. `tier_enabled` = any shard tiers (they
    /// all do or none does — the config is process-wide).
    pub tier_enabled: bool,
    pub tier: TierTotals,
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

/// Retained ops-per-sec samples — 16 × 100 ms default tick ≈ a 1.6 s window.
const OPS_WINDOW: usize = 16;

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
#[derive(Clone, Default)]
pub(crate) struct ReplShardView {
    pub(crate) offset: u64,
    pub(crate) replicas: Vec<kevy_rt::ReplicaViewRow>,
}

impl ObsState {
    pub(crate) fn new(audit_log_path: &Path, nshards: usize) -> Self {
        Self {
            audit: open_audit_log(audit_log_path),
            shard_stats: (0..nshards.max(1))
                .map(|_| Arc::new(ShardStats::default()))
                .collect(),
            ops_ring: Mutex::new(Vec::new()),
            repl_views: (0..nshards.max(1))
                .map(|_| Mutex::new(ReplShardView::default()))
                .collect(),
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
        self.repl_views
            .iter()
            .map(|s| s.lock().expect("repl_views poisoned").clone())
            .collect()
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
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0);
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
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
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
}

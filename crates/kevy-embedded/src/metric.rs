//! Optional push-style metric callback. In-process embed mode has no metrics
//! endpoint, so persistence events (AOF replay on startup, AOF rewrite/
//! compaction) are pushed to a caller-supplied sink — wire it to Prometheus,
//! a log line, a counter, whatever. Wire it via [`crate::Config::with_metric_sink`].

use std::path::PathBuf;
use std::sync::Arc;

/// What `Store::open` restored — and what it could not. The pull-style
/// twin of [`KevyMetric::Replay`] (`Store::open_report()`), so a host can
/// turn "the AOF lost bytes at boot" into a health-check verdict without
/// wiring a metric sink or scraping stderr.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct OpenReport {
    /// Commands replayed from the AOF(s), summed across shards.
    pub replayed_commands: u64,
    /// Bytes actually replayed (the valid prefixes).
    pub replayed_bytes: u64,
    /// Wall-clock time of the whole startup replay, in milliseconds.
    pub elapsed_ms: u64,
    /// Bytes dropped past the last replayable frame, summed across shards.
    /// Non-zero = the store recovered less than the files held.
    pub dropped_bytes: u64,
    /// True when any shard's replay stopped at a corrupt frame.
    pub corrupt: bool,
    /// Quarantine files written while repairing dropped tails (one per
    /// affected shard).
    pub quarantine_paths: Vec<PathBuf>,
    /// Bytes the resync replay hopped over (corrupt regions between valid
    /// records, summed across shards). Zero under strict replay.
    pub resynced_bytes: u64,
}

/// A persistence event worth observing. More variants may be added; match
/// non-exhaustively (`_ => {}`) to stay forward-compatible.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum KevyMetric {
    /// AOF replay finished on startup. `bytes` is the AOF size replayed.
    /// Fires once per `Store::open`, totals summed across all shards.
    Replay {
        /// Commands replayed from the AOF(s).
        commands: u64,
        /// Size in bytes of the AOF file(s) replayed (measured before replay).
        bytes: u64,
        /// Wall-clock time of the whole startup replay, in milliseconds.
        elapsed_ms: u64,
        /// Bytes past the last replayable frame, summed across shards —
        /// dropped from the live file (and quarantined). Non-zero means the
        /// store recovered LESS than the file held: alert on this. A
        /// 3-day production silent-loss incident was exactly this signal
        /// living only in stderr.
        dropped_bytes: u64,
        /// True when any shard's replay stopped at a corrupt frame (vs a
        /// clean end or a partial trailing frame).
        corrupt: bool,
    },
    /// An AOF rewrite (compaction) completed. `before_bytes - after_bytes` is
    /// the space reclaimed. Fires once per rewritten shard.
    Rewrite {
        /// Keys written into the compacted AOF.
        keys: u64,
        /// AOF size in bytes before the rewrite.
        before_bytes: u64,
        /// AOF size in bytes after the rewrite (the compacted log).
        after_bytes: u64,
        /// Wall-clock time of this shard's rewrite, in milliseconds.
        elapsed_ms: u64,
    },
}

/// Cloneable handle to the caller's metric callback. Cheap `Arc` clone; the
/// callback runs synchronously on whichever thread emits the event (the reaper
/// thread for background rewrites, the opening thread for replay), so keep it
/// fast / non-blocking.
#[derive(Clone)]
pub(crate) struct MetricSink(Arc<dyn Fn(KevyMetric) + Send + Sync>);

impl MetricSink {
    pub(crate) fn new(f: impl Fn(KevyMetric) + Send + Sync + 'static) -> Self {
        MetricSink(Arc::new(f))
    }

    pub(crate) fn emit(&self, m: KevyMetric) {
        (self.0)(m);
    }
}

impl std::fmt::Debug for MetricSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MetricSink(<fn>)")
    }
}

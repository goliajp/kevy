//! Optional push-style metric callback. In-process embed mode has no metrics
//! endpoint, so persistence events (AOF replay on startup, AOF rewrite/
//! compaction) are pushed to a caller-supplied sink — wire it to Prometheus,
//! a log line, a counter, whatever. Wire it via [`crate::Config::with_metric_sink`].

use std::sync::Arc;

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

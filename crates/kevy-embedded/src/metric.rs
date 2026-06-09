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
    Replay {
        commands: u64,
        bytes: u64,
        elapsed_ms: u64,
    },
    /// An AOF rewrite (compaction) completed. `before_bytes - after_bytes` is
    /// the space reclaimed.
    Rewrite {
        keys: u64,
        before_bytes: u64,
        after_bytes: u64,
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

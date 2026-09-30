//! The push-style half of the metric surface: [`KevyMetric`] events and
//! the sink that carries them (child module via `#[path]`, split from
//! `metric.rs` for the 500-LOC house rule; behaviour unchanged).

use std::sync::Arc;

/// A persistence event worth observing. More variants may be added; match
/// non-exhaustively (`_ => {}`) to stay forward-compatible.
///
/// ```
/// use std::sync::{Arc, Mutex};
/// use kevy_embedded::{Config, KevyMetric, Store};
///
/// let dir = kevy_tmpdir::TmpDir::new("metric-enum-doc");
/// let seen = Arc::new(Mutex::new(Vec::new()));
/// let sink = Arc::clone(&seen);
/// let cfg = Config::default()
///     .with_persist(dir.path())
///     .with_metric_sink(move |m| sink.lock().unwrap().push(m));
/// let s = Store::open(cfg)?;
/// s.set(b"k", b"v")?;
/// s.rewrite_aof()?;
/// let seen = seen.lock().unwrap();
/// assert!(matches!(seen[0], KevyMetric::Replay { .. }));
/// assert!(matches!(seen[1], KevyMetric::Rewrite { keys: 1, .. }));
/// # Ok::<(), kevy_embedded::KevyError>(())
/// ```
#[cfg(feature = "persist")]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum KevyMetric {
    /// AOF replay finished on startup. `bytes` is the AOF size replayed.
    /// Fires once per `Store::open`, totals summed across all shards.
    ///
    /// ```
    /// # use std::sync::{Arc, Mutex};
    /// # use kevy_embedded::{AppendFsync, Config, KevyMetric, Store};
    /// # let dir = kevy_tmpdir::TmpDir::new("metric-doc");
    /// # let seen = Arc::new(Mutex::new(Vec::new()));
    /// # let cfg = || {
    /// #     let sink = Arc::clone(&seen);
    /// #     Config::default()
    /// #         .with_persist(dir.path())
    /// #         .with_appendfsync(AppendFsync::Always)
    /// #         .with_metric_sink(move |m| sink.lock().unwrap().push(m))
    /// # };
    /// # let s = Store::open(cfg())?;
    /// # s.set(b"a", b"1")?;
    /// # s.set(b"b", b"2")?;
    /// # drop(s);
    /// # let s = Store::open(cfg())?;
    /// let events = seen.lock().unwrap();
    /// let Some(KevyMetric::Replay { commands, dropped_bytes, corrupt, .. }) = events.last().cloned() else {
    ///     panic!("the reopen reports its replay")
    /// };
    /// assert_eq!((commands, dropped_bytes, corrupt), (2, 0, false));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Replay {
        /// Commands replayed from the AOF(s).
        ///
        /// ```
        /// # use std::sync::{Arc, Mutex};
        /// # use kevy_embedded::{AppendFsync, Config, KevyMetric, Store};
        /// # let dir = kevy_tmpdir::TmpDir::new("metric-doc");
        /// # let seen = Arc::new(Mutex::new(Vec::new()));
        /// # let cfg = || {
        /// #     let sink = Arc::clone(&seen);
        /// #     Config::default()
        /// #         .with_persist(dir.path())
        /// #         .with_appendfsync(AppendFsync::Always)
        /// #         .with_metric_sink(move |m| sink.lock().unwrap().push(m))
        /// # };
        /// # let s = Store::open(cfg())?;
        /// # s.set(b"a", b"1")?;
        /// # s.set(b"b", b"2")?;
        /// # drop(s);
        /// # let s = Store::open(cfg())?;
        /// let events = seen.lock().unwrap();
        /// let Some(KevyMetric::Replay { commands, .. }) = events.last().cloned() else {
        ///     panic!("the reopen reports its replay")
        /// };
        /// assert_eq!(commands, 2); // the two SETs of the first open
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        commands: u64,
        /// Size in bytes of the AOF file(s) replayed (measured before replay).
        ///
        /// ```
        /// # use std::sync::{Arc, Mutex};
        /// # use kevy_embedded::{AppendFsync, Config, KevyMetric, Store};
        /// # let dir = kevy_tmpdir::TmpDir::new("metric-doc");
        /// # let seen = Arc::new(Mutex::new(Vec::new()));
        /// # let cfg = || {
        /// #     let sink = Arc::clone(&seen);
        /// #     Config::default()
        /// #         .with_persist(dir.path())
        /// #         .with_appendfsync(AppendFsync::Always)
        /// #         .with_metric_sink(move |m| sink.lock().unwrap().push(m))
        /// # };
        /// # let s = Store::open(cfg())?;
        /// # s.set(b"a", b"1")?;
        /// # s.set(b"b", b"2")?;
        /// # drop(s);
        /// # let s = Store::open(cfg())?;
        /// let events = seen.lock().unwrap();
        /// let Some(KevyMetric::Replay { bytes, .. }) = events.last().cloned() else {
        ///     panic!("the reopen reports its replay")
        /// };
        /// assert_eq!(bytes, std::fs::metadata(dir.path().join("aof-0.aof"))?.len());
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        bytes: u64,
        /// Wall-clock time of the whole startup replay, in milliseconds.
        ///
        /// ```
        /// # use std::sync::{Arc, Mutex};
        /// # use kevy_embedded::{AppendFsync, Config, KevyMetric, Store};
        /// # let dir = kevy_tmpdir::TmpDir::new("metric-doc");
        /// # let seen = Arc::new(Mutex::new(Vec::new()));
        /// # let cfg = || {
        /// #     let sink = Arc::clone(&seen);
        /// #     Config::default()
        /// #         .with_persist(dir.path())
        /// #         .with_appendfsync(AppendFsync::Always)
        /// #         .with_metric_sink(move |m| sink.lock().unwrap().push(m))
        /// # };
        /// # let s = Store::open(cfg())?;
        /// # s.set(b"a", b"1")?;
        /// # s.set(b"b", b"2")?;
        /// # drop(s);
        /// # let s = Store::open(cfg())?;
        /// let events = seen.lock().unwrap();
        /// let Some(KevyMetric::Replay { elapsed_ms, .. }) = events.last().cloned() else {
        ///     panic!("the reopen reports its replay")
        /// };
        /// assert!(elapsed_ms < 60_000); // a two-record log replays at once
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        elapsed_ms: u64,
        /// Bytes past the last replayable frame, summed across shards —
        /// dropped from the live file (and quarantined). Non-zero means the
        /// store recovered LESS than the file held: alert on this. A
        /// 3-day production silent-loss incident was exactly this signal
        /// living only in stderr.
        ///
        /// ```
        /// # use std::sync::{Arc, Mutex};
        /// # use kevy_embedded::{AppendFsync, Config, KevyMetric, Store};
        /// # let dir = kevy_tmpdir::TmpDir::new("metric-doc");
        /// # let seen = Arc::new(Mutex::new(Vec::new()));
        /// # let cfg = || {
        /// #     let sink = Arc::clone(&seen);
        /// #     Config::default()
        /// #         .with_persist(dir.path())
        /// #         .with_appendfsync(AppendFsync::Always)
        /// #         .with_metric_sink(move |m| sink.lock().unwrap().push(m))
        /// # };
        /// # let s = Store::open(cfg())?;
        /// # s.set(b"a", b"1")?;
        /// # s.set(b"b", b"2")?;
        /// # drop(s);
        /// # let s = Store::open(cfg())?;
        /// let events = seen.lock().unwrap();
        /// let Some(KevyMetric::Replay { dropped_bytes, .. }) = events.last().cloned() else {
        ///     panic!("the reopen reports its replay")
        /// };
        /// assert_eq!(dropped_bytes, 0); // a clean close leaves nothing to drop
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        dropped_bytes: u64,
        /// True when any shard's replay stopped at a corrupt frame (vs a
        /// clean end or a partial trailing frame).
        ///
        /// ```
        /// # use std::sync::{Arc, Mutex};
        /// # use kevy_embedded::{AppendFsync, Config, KevyMetric, Store};
        /// # let dir = kevy_tmpdir::TmpDir::new("metric-doc");
        /// # let seen = Arc::new(Mutex::new(Vec::new()));
        /// # let cfg = || {
        /// #     let sink = Arc::clone(&seen);
        /// #     Config::default()
        /// #         .with_persist(dir.path())
        /// #         .with_appendfsync(AppendFsync::Always)
        /// #         .with_metric_sink(move |m| sink.lock().unwrap().push(m))
        /// # };
        /// # let s = Store::open(cfg())?;
        /// # s.set(b"a", b"1")?;
        /// # s.set(b"b", b"2")?;
        /// # drop(s);
        /// # let s = Store::open(cfg())?;
        /// let events = seen.lock().unwrap();
        /// let Some(KevyMetric::Replay { corrupt, .. }) = events.last().cloned() else {
        ///     panic!("the reopen reports its replay")
        /// };
        /// assert!(!corrupt);
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        corrupt: bool,
    },
    /// An AOF rewrite (compaction) completed. `before_bytes - after_bytes` is
    /// the space reclaimed. Fires once per rewritten shard.
    ///
    /// ```
    /// # use std::sync::{Arc, Mutex};
    /// # use kevy_embedded::{AppendFsync, Config, KevyMetric, Store};
    /// # let dir = kevy_tmpdir::TmpDir::new("metric-doc");
    /// # let seen = Arc::new(Mutex::new(Vec::new()));
    /// # let cfg = || {
    /// #     let sink = Arc::clone(&seen);
    /// #     Config::default()
    /// #         .with_persist(dir.path())
    /// #         .with_appendfsync(AppendFsync::Always)
    /// #         .with_metric_sink(move |m| sink.lock().unwrap().push(m))
    /// # };
    /// # let s = Store::open(cfg())?;
    /// for n in 0..100 {
    ///     s.set(b"counter", n.to_string().as_bytes())?;
    /// }
    /// s.rewrite_aof()?;
    /// let events = seen.lock().unwrap();
    /// let Some(KevyMetric::Rewrite { keys, before_bytes, after_bytes, .. }) = events.last().cloned() else {
    ///     panic!("the rewrite reports itself")
    /// };
    /// assert_eq!(keys, 1); // a hundred SETs of one key compact to one
    /// assert!(after_bytes < before_bytes);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Rewrite {
        /// Keys written into the compacted AOF.
        ///
        /// ```
        /// # use std::sync::{Arc, Mutex};
        /// # use kevy_embedded::{AppendFsync, Config, KevyMetric, Store};
        /// # let dir = kevy_tmpdir::TmpDir::new("metric-doc");
        /// # let seen = Arc::new(Mutex::new(Vec::new()));
        /// # let cfg = || {
        /// #     let sink = Arc::clone(&seen);
        /// #     Config::default()
        /// #         .with_persist(dir.path())
        /// #         .with_appendfsync(AppendFsync::Always)
        /// #         .with_metric_sink(move |m| sink.lock().unwrap().push(m))
        /// # };
        /// # let s = Store::open(cfg())?;
        /// for n in 0..100 {
        ///     s.set(b"counter", n.to_string().as_bytes())?;
        /// }
        /// s.rewrite_aof()?;
        /// let events = seen.lock().unwrap();
        /// let Some(KevyMetric::Rewrite { keys, .. }) = events.last().cloned() else {
        ///     panic!("the rewrite reports itself")
        /// };
        /// assert_eq!(keys, 1);
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        keys: u64,
        /// AOF size in bytes before the rewrite.
        ///
        /// ```
        /// # use std::sync::{Arc, Mutex};
        /// # use kevy_embedded::{AppendFsync, Config, KevyMetric, Store};
        /// # let dir = kevy_tmpdir::TmpDir::new("metric-doc");
        /// # let seen = Arc::new(Mutex::new(Vec::new()));
        /// # let cfg = || {
        /// #     let sink = Arc::clone(&seen);
        /// #     Config::default()
        /// #         .with_persist(dir.path())
        /// #         .with_appendfsync(AppendFsync::Always)
        /// #         .with_metric_sink(move |m| sink.lock().unwrap().push(m))
        /// # };
        /// # let s = Store::open(cfg())?;
        /// for n in 0..100 {
        ///     s.set(b"counter", n.to_string().as_bytes())?;
        /// }
        /// s.rewrite_aof()?;
        /// let events = seen.lock().unwrap();
        /// let Some(KevyMetric::Rewrite { before_bytes, after_bytes, .. }) = events.last().cloned() else {
        ///     panic!("the rewrite reports itself")
        /// };
        /// assert!(before_bytes > after_bytes); // the hundred records before compaction
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        before_bytes: u64,
        /// AOF size in bytes after the rewrite (the compacted log).
        ///
        /// ```
        /// # use std::sync::{Arc, Mutex};
        /// # use kevy_embedded::{AppendFsync, Config, KevyMetric, Store};
        /// # let dir = kevy_tmpdir::TmpDir::new("metric-doc");
        /// # let seen = Arc::new(Mutex::new(Vec::new()));
        /// # let cfg = || {
        /// #     let sink = Arc::clone(&seen);
        /// #     Config::default()
        /// #         .with_persist(dir.path())
        /// #         .with_appendfsync(AppendFsync::Always)
        /// #         .with_metric_sink(move |m| sink.lock().unwrap().push(m))
        /// # };
        /// # let s = Store::open(cfg())?;
        /// for n in 0..100 {
        ///     s.set(b"counter", n.to_string().as_bytes())?;
        /// }
        /// s.rewrite_aof()?;
        /// let events = seen.lock().unwrap();
        /// let Some(KevyMetric::Rewrite { before_bytes, after_bytes, .. }) = events.last().cloned() else {
        ///     panic!("the rewrite reports itself")
        /// };
        /// assert!(after_bytes < before_bytes / 10); // one record where there were a hundred
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        after_bytes: u64,
        /// Wall-clock time of this shard's rewrite, in milliseconds.
        ///
        /// ```
        /// # use std::sync::{Arc, Mutex};
        /// # use kevy_embedded::{AppendFsync, Config, KevyMetric, Store};
        /// # let dir = kevy_tmpdir::TmpDir::new("metric-doc");
        /// # let seen = Arc::new(Mutex::new(Vec::new()));
        /// # let cfg = || {
        /// #     let sink = Arc::clone(&seen);
        /// #     Config::default()
        /// #         .with_persist(dir.path())
        /// #         .with_appendfsync(AppendFsync::Always)
        /// #         .with_metric_sink(move |m| sink.lock().unwrap().push(m))
        /// # };
        /// # let s = Store::open(cfg())?;
        /// for n in 0..100 {
        ///     s.set(b"counter", n.to_string().as_bytes())?;
        /// }
        /// s.rewrite_aof()?;
        /// let events = seen.lock().unwrap();
        /// let Some(KevyMetric::Rewrite { elapsed_ms, .. }) = events.last().cloned() else {
        ///     panic!("the rewrite reports itself")
        /// };
        /// assert!(elapsed_ms < 60_000);
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        elapsed_ms: u64,
    },
}

/// Cloneable handle to the caller's metric callback. Cheap `Arc` clone; the
/// callback runs synchronously on whichever thread emits the event (the reaper
/// thread for background rewrites, the opening thread for replay), so keep it
/// fast / non-blocking.
#[cfg(feature = "persist")]
#[derive(Clone)]
pub(crate) struct MetricSink(Arc<dyn Fn(KevyMetric) + Send + Sync>);

#[cfg(feature = "persist")]
impl MetricSink {
    pub(crate) fn new(f: impl Fn(KevyMetric) + Send + Sync + 'static) -> Self {
        MetricSink(Arc::new(f))
    }

    pub(crate) fn emit(&self, m: KevyMetric) {
        (self.0)(m);
    }
}

#[cfg(feature = "persist")]
impl std::fmt::Debug for MetricSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MetricSink(<fn>)")
    }
}

//! Optional push-style metric callback. In-process embed mode has no metrics
//! endpoint, so persistence events (AOF replay on startup, AOF rewrite/
//! compaction) are pushed to a caller-supplied sink — wire it to Prometheus,
//! a log line, a counter, whatever. Wire it via [`crate::Config::with_metric_sink`].

use std::path::PathBuf;

/// What `Store::open` restored — and what it could not. The pull-style
/// twin of [`KevyMetric::Replay`] (`Store::open_report()`), so a host can
/// turn "the AOF lost bytes at boot" into a health-check verdict without
/// wiring a metric sink or scraping stderr.
///
/// ```
/// let s = kevy_embedded::Store::open(kevy_embedded::Config::default())?;
/// let r = s.open_report();
/// assert_eq!((r.replayed_commands, r.dropped_bytes, r.corrupt), (0, 0, false));
/// # Ok::<(), kevy_embedded::KevyError>(())
/// ```
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct OpenReport {
    /// Commands replayed from the AOF(s), summed across shards.
    ///
    /// ```
    /// # use kevy_embedded::{AppendFsync, Config, Store};
    /// # let dir = kevy_tmpdir::TmpDir::new("open-report-doc");
    /// # let cfg = || Config::default().with_persist(dir.path()).with_appendfsync(AppendFsync::Always);
    /// # let s = Store::open(cfg())?;
    /// # s.set(b"a", b"1")?;
    /// # s.set(b"b", b"2")?;
    /// # drop(s);
    /// # let s = Store::open(cfg())?;
    /// # let r = s.open_report();
    /// assert_eq!(r.replayed_commands, 2); // the two SETs came back from the log
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub replayed_commands: u64,
    /// Bytes actually replayed (the valid prefixes).
    ///
    /// ```
    /// # use kevy_embedded::{AppendFsync, Config, Store};
    /// # let dir = kevy_tmpdir::TmpDir::new("open-report-doc");
    /// # let cfg = || Config::default().with_persist(dir.path()).with_appendfsync(AppendFsync::Always);
    /// # let s = Store::open(cfg())?;
    /// # s.set(b"a", b"1")?;
    /// # s.set(b"b", b"2")?;
    /// # drop(s);
    /// # let s = Store::open(cfg())?;
    /// # let r = s.open_report();
    /// let log = std::fs::metadata(dir.path().join("aof-0.aof"))?.len();
    /// assert!(r.replayed_bytes > 0 && r.replayed_bytes <= log);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub replayed_bytes: u64,
    /// Wall-clock time of the whole startup replay, in milliseconds.
    ///
    /// ```
    /// # use kevy_embedded::{AppendFsync, Config, Store};
    /// # let dir = kevy_tmpdir::TmpDir::new("open-report-doc");
    /// # let cfg = || Config::default().with_persist(dir.path()).with_appendfsync(AppendFsync::Always);
    /// # let s = Store::open(cfg())?;
    /// # s.set(b"a", b"1")?;
    /// # s.set(b"b", b"2")?;
    /// # drop(s);
    /// # let s = Store::open(cfg())?;
    /// # let r = s.open_report();
    /// assert!(r.elapsed_ms < 60_000); // a two-record log replays at once
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub elapsed_ms: u64,
    /// Bytes dropped past the last replayable frame, summed across shards.
    /// Non-zero = the store recovered less than the files held.
    ///
    /// ```
    /// # use kevy_embedded::{AppendFsync, Config, Store};
    /// # let dir = kevy_tmpdir::TmpDir::new("open-report-damaged-doc");
    /// # let cfg = || Config::default().with_persist(dir.path()).with_appendfsync(AppendFsync::Always);
    /// # Store::open(cfg())?.set(b"a", b"1")?;
    /// // a record of length 4 whose checksum is wrong, then bytes behind it
    /// let mut log = std::fs::OpenOptions::new().append(true).open(dir.path().join("aof-0.aof"))?;
    /// std::io::Write::write_all(&mut log, &[4, 0, 0, 0, 0, 0, 0, 0, b'a', b'b', b'c', b'd', b'!'])?;
    /// drop(log);
    /// let s = Store::open(cfg())?;
    /// let r = s.open_report();
    /// assert_eq!(r.dropped_bytes, 13); // everything from the bad record on
    /// assert_eq!(s.get(b"a")?.as_deref(), Some(&b"1"[..])); // the good prefix survives
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub dropped_bytes: u64,
    /// True when any shard's replay stopped at a corrupt frame.
    ///
    /// ```
    /// # use kevy_embedded::{AppendFsync, Config, Store};
    /// # let dir = kevy_tmpdir::TmpDir::new("open-report-damaged-doc");
    /// # let cfg = || Config::default().with_persist(dir.path()).with_appendfsync(AppendFsync::Always);
    /// # Store::open(cfg())?.set(b"a", b"1")?;
    /// // a record of length 4 whose checksum is wrong, then bytes behind it
    /// let mut log = std::fs::OpenOptions::new().append(true).open(dir.path().join("aof-0.aof"))?;
    /// std::io::Write::write_all(&mut log, &[4, 0, 0, 0, 0, 0, 0, 0, b'a', b'b', b'c', b'd', b'!'])?;
    /// drop(log);
    /// let s = Store::open(cfg())?;
    /// let r = s.open_report();
    /// assert!(r.corrupt); // stopped at a bad record, not at a torn final one
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub corrupt: bool,
    /// Quarantine files written while repairing dropped tails (one per
    /// affected shard).
    ///
    /// ```
    /// # use kevy_embedded::{AppendFsync, Config, Store};
    /// # let dir = kevy_tmpdir::TmpDir::new("open-report-damaged-doc");
    /// # let cfg = || Config::default().with_persist(dir.path()).with_appendfsync(AppendFsync::Always);
    /// # Store::open(cfg())?.set(b"a", b"1")?;
    /// // a record of length 4 whose checksum is wrong, then bytes behind it
    /// let mut log = std::fs::OpenOptions::new().append(true).open(dir.path().join("aof-0.aof"))?;
    /// std::io::Write::write_all(&mut log, &[4, 0, 0, 0, 0, 0, 0, 0, b'a', b'b', b'c', b'd', b'!'])?;
    /// drop(log);
    /// let s = Store::open(cfg())?;
    /// let r = s.open_report();
    /// assert_eq!(r.quarantine_paths.len(), 1);
    /// assert_eq!(std::fs::metadata(&r.quarantine_paths[0])?.len(), 13); // what was cut off
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub quarantine_paths: Vec<PathBuf>,
    /// Bytes the resync replay hopped over (corrupt regions between valid
    /// records, summed across shards). Zero under strict replay.
    ///
    /// ```
    /// use kevy_embedded::{AppendFsync, Config, ReplayMode, Store};
    ///
    /// let dir = kevy_tmpdir::TmpDir::new("open-report-resync-doc");
    /// let aof = dir.path().join("aof-0.aof");
    /// let cfg = || Config::default().with_persist(dir.path()).with_appendfsync(AppendFsync::Always);
    /// let s = Store::open(cfg())?;
    /// s.set(b"a", b"1")?;
    /// let head = std::fs::read(&aof)?;
    /// s.set(b"b", b"2")?;
    /// drop(s);
    /// let all = std::fs::read(&aof)?;
    /// // a record whose checksum lies, between the two good ones
    /// let bad = [4, 0, 0, 0, 0, 0, 0, 0, b'a', b'b', b'c', b'd'];
    /// std::fs::write(&aof, [&head[..], &bad[..], &all[head.len()..]].concat())?;
    /// let s = Store::open(cfg().with_replay_mode(ReplayMode::Resync))?;
    /// assert_eq!(s.open_report().resynced_bytes, bad.len() as u64);
    /// assert_eq!(s.get(b"b")?.as_deref(), Some(&b"2"[..])); // recovered behind it
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub resynced_bytes: u64,
    /// Writes the staging rings held that the AOFs did not — appended
    /// before the last process was killed and not yet drained — replayed
    /// and appended to the logs at this open, summed across shards.
    ///
    /// ```
    /// # use kevy_embedded::{AppendFsync, Config, Store};
    /// # let dir = kevy_tmpdir::TmpDir::new("open-report-killed-doc");
    /// # let cfg = || {
    /// #     Config::default()
    /// #         .with_persist(dir.path())
    /// #         .with_appendfsync(AppendFsync::EverySec)
    /// #         .with_ttl_reaper_manual()
    /// #         .with_mapped_aof(false)
    /// #         .with_stage_ring(64 * 1024)
    /// # };
    /// # let s = Store::open(cfg())?;
    /// # s.set(b"k", b"v")?;
    /// // stands in for a killed process: no close runs, the lock goes with it
    /// std::mem::forget(s);
    /// std::fs::remove_file(dir.path().join("LOCK"))?;
    /// let s = Store::open(cfg())?;
    /// let r = s.open_report();
    /// assert!(r.stage_recovered > 0); // the ring held the write the log had not
    /// assert_eq!(s.get(b"k")?.as_deref(), Some(&b"v"[..]));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub stage_recovered: u64,
    /// Staging rings set aside because they could not show where they
    /// continue their log; only a power loss or a log changed by hand
    /// leaves one.
    ///
    /// ```
    /// # use kevy_embedded::{AppendFsync, Config, Store};
    /// # let dir = kevy_tmpdir::TmpDir::new("open-report-killed-doc");
    /// # let cfg = || {
    /// #     Config::default()
    /// #         .with_persist(dir.path())
    /// #         .with_appendfsync(AppendFsync::EverySec)
    /// #         .with_ttl_reaper_manual()
    /// #         .with_mapped_aof(false)
    /// #         .with_stage_ring(64 * 1024)
    /// # };
    /// # let s = Store::open(cfg())?;
    /// # s.set(b"k", b"v")?;
    /// // stands in for a killed process: no close runs, the lock goes with it
    /// std::mem::forget(s);
    /// std::fs::remove_file(dir.path().join("LOCK"))?;
    /// let s = Store::open(cfg())?;
    /// let r = s.open_report();
    /// // a kill alone leaves a ring that shows where it continues its log
    /// assert_eq!((r.stage_recovered > 0, r.stage_discarded), (true, 0));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub stage_discarded: u64,
}

#[cfg(feature = "persist")]
#[path = "metric_event.rs"]
mod event;
#[cfg(feature = "persist")]
pub use event::KevyMetric;
#[cfg(feature = "persist")]
pub(crate) use event::MetricSink;

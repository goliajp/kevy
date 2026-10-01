//! Open-report over the C ABI — the machine-readable boot-replay verdict.
//!
//! `Store::open_report()`'s POD projection: counts, the corrupt flag, and
//! how many quarantine files the open's repair wrote (their paths sit next
//! to the AOF, named `aof-<id>.aof.corrupt-quarantine.<ts>`, and the boot
//! WARN line prints them). Split out of `lib.rs` for the 500-LOC house
//! rule; additive, `KEVY_ABI` unchanged.

use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::KevyDb;

/// What the open's replay restored — and what it could not. A nonzero
/// `dropped_bytes` or `corrupt` means the store recovered LESS than its
/// files held (the dropped region was quarantined): surface it as a
/// startup health check.
///
/// ```
/// use kevy_ffi::{KevyOpenReport, kevy_close, kevy_open, kevy_open_report, kevy_set};
///
/// # let dir = std::env::temp_dir().join(format!("kevy-ffi-report-1-{}", std::process::id()));
/// # let path = dir.to_str().ok_or("temp dir is not UTF-8")?;
/// # let mut rep = KevyOpenReport { replayed_commands: 0, replayed_bytes: 0, elapsed_ms: 0,
/// #     dropped_bytes: 0, corrupt: 0, quarantine_count: 0 };
/// // SAFETY: `path` outlives the calls; `rep` is writable; each handle is
/// // closed once.
/// unsafe {
///     let db = kevy_open(path.as_ptr(), path.len());
///     kevy_set(db, b"a".as_ptr(), 1, b"1".as_ptr(), 1, 0);
///     kevy_set(db, b"b".as_ptr(), 1, b"2".as_ptr(), 1, 0);
///     kevy_close(db);
///     let db = kevy_open(path.as_ptr(), path.len());
///     assert_eq!(kevy_open_report(db, &mut rep), 0);
///     kevy_close(db);
/// }
/// let healthy = rep.dropped_bytes == 0 && rep.corrupt == 0;
/// assert!(healthy);
/// std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KevyOpenReport {
    /// Commands replayed from the AOF(s), summed across shards.
    ///
    /// ```
    /// # use kevy_ffi::*;
    /// # let dir = std::env::temp_dir().join(format!("kevy-ffi-report-2-{}", std::process::id()));
    /// # let path = dir.to_str().ok_or("temp dir is not UTF-8")?;
    /// # let mut rep = KevyOpenReport { replayed_commands: 0, replayed_bytes: 0, elapsed_ms: 0,
    /// #     dropped_bytes: 0, corrupt: 0, quarantine_count: 0 };
    /// // SAFETY: `path` outlives the calls; `rep` is writable; each handle is
    /// // closed once.
    /// unsafe {
    ///     let db = kevy_open(path.as_ptr(), path.len());
    ///     kevy_set(db, b"a".as_ptr(), 1, b"1".as_ptr(), 1, 0);
    ///     kevy_set(db, b"b".as_ptr(), 1, b"2".as_ptr(), 1, 0);
    ///     kevy_close(db);
    ///     let db = kevy_open(path.as_ptr(), path.len());
    ///     assert_eq!(kevy_open_report(db, &mut rep), 0);
    ///     kevy_close(db);
    /// }
    /// assert_eq!(rep.replayed_commands, 2); // the two SETs came back from the log
    /// std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub replayed_commands: u64,
    /// Bytes actually replayed (the valid prefixes).
    ///
    /// ```
    /// # use kevy_ffi::*;
    /// # let dir = std::env::temp_dir().join(format!("kevy-ffi-report-3-{}", std::process::id()));
    /// # let path = dir.to_str().ok_or("temp dir is not UTF-8")?;
    /// # let mut rep = KevyOpenReport { replayed_commands: 0, replayed_bytes: 0, elapsed_ms: 0,
    /// #     dropped_bytes: 0, corrupt: 0, quarantine_count: 0 };
    /// // SAFETY: `path` outlives the calls; `rep` is writable; each handle is
    /// // closed once.
    /// unsafe {
    ///     let db = kevy_open(path.as_ptr(), path.len());
    ///     kevy_set(db, b"a".as_ptr(), 1, b"1".as_ptr(), 1, 0);
    ///     kevy_set(db, b"b".as_ptr(), 1, b"2".as_ptr(), 1, 0);
    ///     kevy_close(db);
    ///     let db = kevy_open(path.as_ptr(), path.len());
    ///     assert_eq!(kevy_open_report(db, &mut rep), 0);
    ///     kevy_close(db);
    /// }
    /// let log = std::fs::metadata(dir.join("aof-0.aof"))?.len();
    /// assert!(rep.replayed_bytes > 0 && rep.replayed_bytes <= log);
    /// std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub replayed_bytes: u64,
    /// Wall-clock time of the startup replay, in milliseconds.
    ///
    /// ```
    /// # use kevy_ffi::*;
    /// # let dir = std::env::temp_dir().join(format!("kevy-ffi-report-4-{}", std::process::id()));
    /// # let path = dir.to_str().ok_or("temp dir is not UTF-8")?;
    /// # let mut rep = KevyOpenReport { replayed_commands: 0, replayed_bytes: 0, elapsed_ms: 0,
    /// #     dropped_bytes: 0, corrupt: 0, quarantine_count: 0 };
    /// // SAFETY: `path` outlives the calls; `rep` is writable; each handle is
    /// // closed once.
    /// unsafe {
    ///     let db = kevy_open(path.as_ptr(), path.len());
    ///     kevy_set(db, b"a".as_ptr(), 1, b"1".as_ptr(), 1, 0);
    ///     kevy_set(db, b"b".as_ptr(), 1, b"2".as_ptr(), 1, 0);
    ///     kevy_close(db);
    ///     let db = kevy_open(path.as_ptr(), path.len());
    ///     assert_eq!(kevy_open_report(db, &mut rep), 0);
    ///     kevy_close(db);
    /// }
    /// assert!(rep.elapsed_ms < 60_000); // a two-record log replays at once
    /// std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub elapsed_ms: u64,
    /// Bytes dropped past the last replayable frame (quarantined).
    ///
    /// ```
    /// # use kevy_ffi::*;
    /// # let dir = std::env::temp_dir().join(format!("kevy-ffi-report-bad-5-{}", std::process::id()));
    /// # let path = dir.to_str().ok_or("temp dir is not UTF-8")?;
    /// # let mut rep = KevyOpenReport { replayed_commands: 0, replayed_bytes: 0, elapsed_ms: 0,
    /// #     dropped_bytes: 0, corrupt: 0, quarantine_count: 0 };
    /// # // SAFETY: `path` outlives the calls; each handle is closed once.
    /// # unsafe {
    /// #     let db = kevy_open(path.as_ptr(), path.len());
    /// #     kevy_set(db, b"a".as_ptr(), 1, b"1".as_ptr(), 1, 0);
    /// #     kevy_close(db);
    /// # }
    /// // a record of length 4 whose checksum is wrong, then bytes behind it
    /// let mut log = std::fs::OpenOptions::new().append(true).open(dir.join("aof-0.aof"))?;
    /// std::io::Write::write_all(&mut log, &[4, 0, 0, 0, 0, 0, 0, 0, b'a', b'b', b'c', b'd', b'!'])?;
    /// drop(log);
    /// // SAFETY: `path` outlives the calls; `rep` is writable; closed once.
    /// unsafe {
    ///     let db = kevy_open(path.as_ptr(), path.len());
    ///     assert_eq!(kevy_open_report(db, &mut rep), 0);
    ///     kevy_close(db);
    /// }
    /// assert_eq!(rep.dropped_bytes, 13); // everything from the bad record on
    /// std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub dropped_bytes: u64,
    /// 1 when any shard's replay stopped at a corrupt frame.
    ///
    /// ```
    /// # use kevy_ffi::*;
    /// # let dir = std::env::temp_dir().join(format!("kevy-ffi-report-bad-6-{}", std::process::id()));
    /// # let path = dir.to_str().ok_or("temp dir is not UTF-8")?;
    /// # let mut rep = KevyOpenReport { replayed_commands: 0, replayed_bytes: 0, elapsed_ms: 0,
    /// #     dropped_bytes: 0, corrupt: 0, quarantine_count: 0 };
    /// # // SAFETY: `path` outlives the calls; each handle is closed once.
    /// # unsafe {
    /// #     let db = kevy_open(path.as_ptr(), path.len());
    /// #     kevy_set(db, b"a".as_ptr(), 1, b"1".as_ptr(), 1, 0);
    /// #     kevy_close(db);
    /// # }
    /// // a record of length 4 whose checksum is wrong, then bytes behind it
    /// let mut log = std::fs::OpenOptions::new().append(true).open(dir.join("aof-0.aof"))?;
    /// std::io::Write::write_all(&mut log, &[4, 0, 0, 0, 0, 0, 0, 0, b'a', b'b', b'c', b'd', b'!'])?;
    /// drop(log);
    /// // SAFETY: `path` outlives the calls; `rep` is writable; closed once.
    /// unsafe {
    ///     let db = kevy_open(path.as_ptr(), path.len());
    ///     assert_eq!(kevy_open_report(db, &mut rep), 0);
    ///     kevy_close(db);
    /// }
    /// assert_eq!(rep.corrupt, 1); // stopped at a bad record, not at a torn final one
    /// std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub corrupt: u8,
    /// Quarantine files written by the open's repair (one per affected
    /// shard).
    ///
    /// ```
    /// # use kevy_ffi::*;
    /// # let dir = std::env::temp_dir().join(format!("kevy-ffi-report-bad-7-{}", std::process::id()));
    /// # let path = dir.to_str().ok_or("temp dir is not UTF-8")?;
    /// # let mut rep = KevyOpenReport { replayed_commands: 0, replayed_bytes: 0, elapsed_ms: 0,
    /// #     dropped_bytes: 0, corrupt: 0, quarantine_count: 0 };
    /// # // SAFETY: `path` outlives the calls; each handle is closed once.
    /// # unsafe {
    /// #     let db = kevy_open(path.as_ptr(), path.len());
    /// #     kevy_set(db, b"a".as_ptr(), 1, b"1".as_ptr(), 1, 0);
    /// #     kevy_close(db);
    /// # }
    /// // a record of length 4 whose checksum is wrong, then bytes behind it
    /// let mut log = std::fs::OpenOptions::new().append(true).open(dir.join("aof-0.aof"))?;
    /// std::io::Write::write_all(&mut log, &[4, 0, 0, 0, 0, 0, 0, 0, b'a', b'b', b'c', b'd', b'!'])?;
    /// drop(log);
    /// // SAFETY: `path` outlives the calls; `rep` is writable; closed once.
    /// unsafe {
    ///     let db = kevy_open(path.as_ptr(), path.len());
    ///     assert_eq!(kevy_open_report(db, &mut rep), 0);
    ///     kevy_close(db);
    /// }
    /// assert_eq!(rep.quarantine_count, 1);
    /// let quarantined = std::fs::read_dir(&dir)?
    ///     .filter(|e| e.as_ref().is_ok_and(|e| e.file_name().to_string_lossy().contains("quarantine")))
    ///     .count();
    /// assert_eq!(quarantined, 1); // the cut-off bytes, kept beside the log
    /// std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub quarantine_count: u32,
}

/// Fill `*out` with the boot-replay verdict of `db`'s open. Returns 0 on
/// success, -1 on misuse (null handle/out).
///
/// ```
/// use kevy_ffi::{KevyOpenReport, kevy_close, kevy_open_mem, kevy_open_report};
///
/// let mut rep = KevyOpenReport { replayed_commands: 9, replayed_bytes: 9, elapsed_ms: 9,
///     dropped_bytes: 9, corrupt: 9, quarantine_count: 9 };
/// let db = kevy_open_mem();
/// // SAFETY: `db` is live and `rep` writable; `db` is closed once.
/// unsafe {
///     assert_eq!(kevy_open_report(db, &mut rep), 0);
///     assert_eq!(kevy_open_report(std::ptr::null_mut(), &mut rep), -1);
///     kevy_close(db);
/// }
/// // nothing to replay in memory: every count is overwritten with zero
/// assert_eq!((rep.replayed_commands, rep.dropped_bytes, rep.corrupt), (0, 0, 0));
/// ```
///
/// # Safety
/// `db` must be a live handle from `kevy_open*`; `out` must point to a
/// writable [`KevyOpenReport`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_open_report(db: *mut KevyDb, out: *mut KevyOpenReport) -> i32 {
    if db.is_null() || out.is_null() {
        return -1;
    }
    // SAFETY: checked non-null above; the contract requires a live `kevy_open*` handle.
    let store = unsafe { &(*db).store };
    let filled = catch_unwind(AssertUnwindSafe(|| {
        let r = store.open_report();
        KevyOpenReport {
            replayed_commands: r.replayed_commands,
            replayed_bytes: r.replayed_bytes,
            elapsed_ms: r.elapsed_ms,
            dropped_bytes: r.dropped_bytes,
            corrupt: u8::from(r.corrupt),
            quarantine_count: r.quarantine_paths.len() as u32,
        }
    }));
    match filled {
        Ok(rep) => {
            // SAFETY: covered by this fn's `# Safety` contract, with the null case ruled out by
            // the checks above.
            unsafe { out.write(rep) };
            0
        }
        Err(_) => -1,
    }
}

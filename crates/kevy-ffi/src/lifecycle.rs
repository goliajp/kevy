//! Lifecycle extras over the C ABI: `kevy_open_with` (open with explicit
//! durability/rewrite policy — the knobs `kevy_open` locks to defaults)
//! and `kevy_shutdown` (the deterministic two-line teardown). Split from
//! `lib.rs` for the 500-LOC house rule; additive, `KEVY_ABI` unchanged.

use std::panic::{AssertUnwindSafe, catch_unwind};

use kevy_embedded::{AppendFsync, Config};

use crate::{KevyDb, open_with};

/// Options for [`kevy_open_with`]. Start from `KEVY_OPEN_OPTIONS_INIT`
/// (the header's initializer — the exact defaults `kevy_open` uses) and
/// override what you need; a zero-initialized struct instead disables
/// auto-rewrite entirely (`rewrite_pct = 0` is the off switch, as in
/// Redis).
///
/// ```
/// use kevy_ffi::{KevyOpenOptions, kevy_close, kevy_open_with};
///
/// // the defaults `kevy_open` uses, but with every write fsynced
/// let opts = KevyOpenOptions {
///     fsync: 1,
///     shards: 0,
///     rewrite_pct: 100,
///     rewrite_min_size: 64 << 20,
///     rewrite_bytes: 0,
///     rewrite_interval_secs: 0,
/// };
/// // SAFETY: null dir with length 0 = in memory; `opts` is readable; closed once.
/// unsafe {
///     let db = kevy_open_with(std::ptr::null(), 0, &opts);
///     assert!(!db.is_null());
///     kevy_close(db);
/// }
/// ```
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KevyOpenOptions {
    /// 0 = everysec (default), 1 = always, 2 = no.
    ///
    /// ```
    /// use kevy_ffi::{KevyOpenOptions, kevy_close, kevy_open_with, kevy_set};
    /// # let defaults = KevyOpenOptions { fsync: 0, shards: 0, rewrite_pct: 100,
    /// #     rewrite_min_size: 64 << 20, rewrite_bytes: 0, rewrite_interval_secs: 0 };
    /// let dir = std::env::temp_dir().join(format!("kevy-ffi-fsync-doc-{}", std::process::id()));
    /// let path = dir.to_str().ok_or("temp dir is not UTF-8")?;
    /// let opts = KevyOpenOptions { fsync: 1, ..defaults };
    /// // SAFETY: `path` and `opts` outlive the calls; the handle is closed once.
    /// unsafe {
    ///     let db = kevy_open_with(path.as_ptr(), path.len(), &opts);
    ///     kevy_set(db, b"k".as_ptr(), 1, b"v".as_ptr(), 1, 0);
    ///     // "always": the record is in the file before the call returns
    ///     assert!(std::fs::metadata(dir.join("aof-0.aof"))?.len() > 0);
    ///     kevy_close(db);
    /// }
    /// std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fsync: u8,
    /// Keyspace shards (0 = default, 1).
    ///
    /// ```
    /// use kevy_ffi::{KevyOpenOptions, kevy_close, kevy_open_with};
    /// # let defaults = KevyOpenOptions { fsync: 0, shards: 0, rewrite_pct: 100,
    /// #     rewrite_min_size: 64 << 20, rewrite_bytes: 0, rewrite_interval_secs: 0 };
    /// let dir = std::env::temp_dir().join(format!("kevy-ffi-shards-doc-{}", std::process::id()));
    /// let path = dir.to_str().ok_or("temp dir is not UTF-8")?;
    /// let opts = KevyOpenOptions { shards: 4, ..defaults };
    /// // SAFETY: `path` and `opts` outlive the calls; the handle is closed once.
    /// unsafe {
    ///     let db = kevy_open_with(path.as_ptr(), path.len(), &opts);
    ///     assert!(dir.join("aof-3.aof").exists()); // one log per shard
    ///     kevy_close(db);
    /// }
    /// std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub shards: u32,
    /// Auto-rewrite growth trigger, percent (0 = rule off; default 100).
    ///
    /// ```
    /// # use kevy_ffi::*;
    /// # fn replayed(tag: &str, opts: KevyOpenOptions, n: usize) -> Result<u64, Box<dyn std::error::Error>> {
    /// #     let dir = std::env::temp_dir().join(format!("kevy-ffi-{tag}-{}", std::process::id()));
    /// #     let path = dir.to_str().ok_or("temp dir is not UTF-8")?;
    /// #     let mut rep = KevyOpenReport { replayed_commands: 0, replayed_bytes: 0, elapsed_ms: 0,
    /// #         dropped_bytes: 0, corrupt: 0, quarantine_count: 0 };
    /// #     // SAFETY: `path` and `opts` outlive the calls; each handle is closed once.
    /// #     unsafe {
    /// #         let db = kevy_open_with(path.as_ptr(), path.len(), &opts);
    /// #         for _ in 0..n {
    /// #             kevy_set(db, b"k".as_ptr(), 1, b"v".as_ptr(), 1, 0);
    /// #         }
    /// #         kevy_close(db);
    /// #         let db = kevy_open_with(path.as_ptr(), path.len(), &opts);
    /// #         kevy_open_report(db, &mut rep);
    /// #         kevy_close(db);
    /// #     }
    /// #     std::fs::remove_dir_all(&dir)?;
    /// #     Ok(rep.replayed_commands)
    /// # }
    /// # let defaults = KevyOpenOptions { fsync: 0, shards: 0, rewrite_pct: 100,
    /// #     rewrite_min_size: 64 << 20, rewrite_bytes: 0, rewrite_interval_secs: 0 };
    /// // rule off: 50 overwrites of one key stay 50 records in the log
    /// let off = KevyOpenOptions { rewrite_pct: 0, ..defaults };
    /// assert_eq!(replayed("pct-doc", off, 50)?, 50);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub rewrite_pct: u32,
    /// Growth rule's minimum size gate (default 64 MiB).
    ///
    /// ```
    /// # use kevy_ffi::*;
    /// # fn replayed(tag: &str, opts: KevyOpenOptions, n: usize) -> Result<u64, Box<dyn std::error::Error>> {
    /// #     let dir = std::env::temp_dir().join(format!("kevy-ffi-{tag}-{}", std::process::id()));
    /// #     let path = dir.to_str().ok_or("temp dir is not UTF-8")?;
    /// #     let mut rep = KevyOpenReport { replayed_commands: 0, replayed_bytes: 0, elapsed_ms: 0,
    /// #         dropped_bytes: 0, corrupt: 0, quarantine_count: 0 };
    /// #     // SAFETY: `path` and `opts` outlive the calls; each handle is closed once.
    /// #     unsafe {
    /// #         let db = kevy_open_with(path.as_ptr(), path.len(), &opts);
    /// #         for _ in 0..n {
    /// #             kevy_set(db, b"k".as_ptr(), 1, b"v".as_ptr(), 1, 0);
    /// #         }
    /// #         kevy_close(db);
    /// #         let db = kevy_open_with(path.as_ptr(), path.len(), &opts);
    /// #         kevy_open_report(db, &mut rep);
    /// #         kevy_close(db);
    /// #     }
    /// #     std::fs::remove_dir_all(&dir)?;
    /// #     Ok(rep.replayed_commands)
    /// # }
    /// # let defaults = KevyOpenOptions { fsync: 0, shards: 0, rewrite_pct: 100,
    /// #     rewrite_min_size: 64 << 20, rewrite_bytes: 0, rewrite_interval_secs: 0 };
    /// // a log far under the gate is never compacted by the growth rule
    /// let gated = KevyOpenOptions { rewrite_min_size: 1 << 30, ..defaults };
    /// assert_eq!(replayed("min-doc", gated, 50)?, 50);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub rewrite_min_size: u64,
    /// Absolute-size trigger (0 = rule off).
    ///
    /// ```
    /// # use kevy_ffi::*;
    /// # fn replayed(tag: &str, opts: KevyOpenOptions, n: usize) -> Result<u64, Box<dyn std::error::Error>> {
    /// #     let dir = std::env::temp_dir().join(format!("kevy-ffi-{tag}-{}", std::process::id()));
    /// #     let path = dir.to_str().ok_or("temp dir is not UTF-8")?;
    /// #     let mut rep = KevyOpenReport { replayed_commands: 0, replayed_bytes: 0, elapsed_ms: 0,
    /// #         dropped_bytes: 0, corrupt: 0, quarantine_count: 0 };
    /// #     // SAFETY: `path` and `opts` outlive the calls; each handle is closed once.
    /// #     unsafe {
    /// #         let db = kevy_open_with(path.as_ptr(), path.len(), &opts);
    /// #         for _ in 0..n {
    /// #             kevy_set(db, b"k".as_ptr(), 1, b"v".as_ptr(), 1, 0);
    /// #         }
    /// #         kevy_close(db);
    /// #         let db = kevy_open_with(path.as_ptr(), path.len(), &opts);
    /// #         kevy_open_report(db, &mut rep);
    /// #         kevy_close(db);
    /// #     }
    /// #     std::fs::remove_dir_all(&dir)?;
    /// #     Ok(rep.replayed_commands)
    /// # }
    /// # let defaults = KevyOpenOptions { fsync: 0, shards: 0, rewrite_pct: 100,
    /// #     rewrite_min_size: 64 << 20, rewrite_bytes: 0, rewrite_interval_secs: 0 };
    /// // a 1 GiB ceiling is nowhere near reached by 50 small writes
    /// let capped = KevyOpenOptions { rewrite_bytes: 1 << 30, ..defaults };
    /// assert_eq!(replayed("bytes-doc", capped, 50)?, 50);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub rewrite_bytes: u64,
    /// Staleness trigger, seconds (0 = rule off).
    ///
    /// ```
    /// # use kevy_ffi::*;
    /// # fn replayed(tag: &str, opts: KevyOpenOptions, n: usize) -> Result<u64, Box<dyn std::error::Error>> {
    /// #     let dir = std::env::temp_dir().join(format!("kevy-ffi-{tag}-{}", std::process::id()));
    /// #     let path = dir.to_str().ok_or("temp dir is not UTF-8")?;
    /// #     let mut rep = KevyOpenReport { replayed_commands: 0, replayed_bytes: 0, elapsed_ms: 0,
    /// #         dropped_bytes: 0, corrupt: 0, quarantine_count: 0 };
    /// #     // SAFETY: `path` and `opts` outlive the calls; each handle is closed once.
    /// #     unsafe {
    /// #         let db = kevy_open_with(path.as_ptr(), path.len(), &opts);
    /// #         for _ in 0..n {
    /// #             kevy_set(db, b"k".as_ptr(), 1, b"v".as_ptr(), 1, 0);
    /// #         }
    /// #         kevy_close(db);
    /// #         let db = kevy_open_with(path.as_ptr(), path.len(), &opts);
    /// #         kevy_open_report(db, &mut rep);
    /// #         kevy_close(db);
    /// #     }
    /// #     std::fs::remove_dir_all(&dir)?;
    /// #     Ok(rep.replayed_commands)
    /// # }
    /// # let defaults = KevyOpenOptions { fsync: 0, shards: 0, rewrite_pct: 100,
    /// #     rewrite_min_size: 64 << 20, rewrite_bytes: 0, rewrite_interval_secs: 0 };
    /// // an hourly staleness rewrite does not fire within this example
    /// let hourly = KevyOpenOptions { rewrite_interval_secs: 3600, ..defaults };
    /// assert_eq!(replayed("interval-doc", hourly, 50)?, 50);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub rewrite_interval_secs: u64,
}

fn apply(opts: &KevyOpenOptions, mut cfg: Config) -> Config {
    cfg = cfg.with_appendfsync(match opts.fsync {
        1 => AppendFsync::Always,
        2 => AppendFsync::No,
        _ => AppendFsync::EverySec,
    });
    if opts.shards > 0 {
        cfg = cfg.with_shards(opts.shards as usize);
    }
    cfg = cfg.with_auto_aof_rewrite(opts.rewrite_pct, opts.rewrite_min_size);
    cfg = cfg.with_auto_rewrite_bytes(opts.rewrite_bytes);
    cfg.with_auto_rewrite_interval(std::time::Duration::from_secs(opts.rewrite_interval_secs))
}

/// [`kevy_open`](crate::kevy_open) with explicit options: durable at `dir` when `dir` is
/// non-null, in-memory when `dir` is null and `dir_len` is 0. A null
/// `opts` behaves exactly like `kevy_open` / `kevy_open_mem`. Returns
/// null on failure.
///
/// ```
/// use kevy_ffi::{KevyOpenOptions, kevy_close, kevy_open_with};
///
/// let opts = KevyOpenOptions { fsync: 0, shards: 2, rewrite_pct: 100,
///     rewrite_min_size: 64 << 20, rewrite_bytes: 0, rewrite_interval_secs: 0 };
/// // SAFETY: null dir with length 0 = in memory; `opts` is readable; closed once.
/// unsafe {
///     let db = kevy_open_with(std::ptr::null(), 0, &opts);
///     assert!(!db.is_null());
///     kevy_close(db);
///     // a null dir with a nonzero length is misuse
///     assert!(kevy_open_with(std::ptr::null(), 3, &opts).is_null());
/// }
/// ```
///
/// # Safety
/// `dir`, when non-null, must point to `dir_len` readable bytes; `opts`,
/// when non-null, must point to a readable [`KevyOpenOptions`].
// NO-UNWIND: the store is opened inside open_with, which catches; the rest is a UTF-8 check and plain field copies
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_open_with(
    dir: *const u8,
    dir_len: usize,
    opts: *const KevyOpenOptions,
) -> *mut KevyDb {
    let base = if dir.is_null() {
        if dir_len != 0 {
            return std::ptr::null_mut();
        }
        Config::default()
    } else {
        // SAFETY: the `# Safety` contract above covers this pointer/length pair.
        let bytes = unsafe { std::slice::from_raw_parts(dir, dir_len) };
        let Ok(path) = std::str::from_utf8(bytes) else {
            return std::ptr::null_mut();
        };
        Config::default().with_persist(path.to_owned())
    };
    // SAFETY: checked non-null above; the contract requires a live `kevy_open*` handle.
    let cfg = if opts.is_null() { base } else { apply(unsafe { &*opts }, base) };
    open_with(move || cfg)
}

/// Flush every shard's AOF with a REAL fsync, write the feed continuity
/// marker, then refuse every later write (reads stay available). The
/// deterministic teardown for a host's signal handler: `kevy_shutdown(db);
/// exit(0)`. Idempotent. Returns 0 on success, -1 on misuse, -2 on an
/// I/O failure (the store is still usable; retry or exit).
///
/// ```
/// use kevy_ffi::{KevyBuf, kevy_buf_free, kevy_close, kevy_get, kevy_open_mem, kevy_set, kevy_shutdown};
///
/// let db = kevy_open_mem();
/// let mut out = KevyBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
/// // SAFETY: `db` is live; key/value pointers cover their lengths; the read
/// // is freed once; `db` is closed once.
/// unsafe {
///     kevy_set(db, b"k".as_ptr(), 1, b"v".as_ptr(), 1, 0);
///     assert_eq!(kevy_shutdown(db), 0);
///     assert!(kevy_set(db, b"k".as_ptr(), 1, b"w".as_ptr(), 1, 0) < 0); // writes refused
///     assert_eq!(kevy_get(db, b"k".as_ptr(), 1, &mut out), 1); // reads still served
///     kevy_buf_free(out.ptr, out.len, out.cap);
///     kevy_close(db);
/// }
/// ```
///
/// # Safety
/// `db` must be a live handle from `kevy_open*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_shutdown(db: *mut KevyDb) -> i32 {
    if db.is_null() {
        return -1;
    }
    // SAFETY: checked non-null above; the contract requires a live `kevy_open*` handle.
    let store = unsafe { &(*db).store };
    match catch_unwind(AssertUnwindSafe(|| store.shutdown())) {
        Ok(Ok(())) => 0,
        Ok(Err(_)) => -2,
        Err(_) => -2,
    }
}

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
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KevyOpenOptions {
    /// 0 = everysec (default), 1 = always, 2 = no.
    pub fsync: u8,
    /// Keyspace shards (0 = default, 1).
    pub shards: u32,
    /// Auto-rewrite growth trigger, percent (0 = rule off; default 100).
    pub rewrite_pct: u32,
    /// Growth rule's minimum size gate (default 64 MiB).
    pub rewrite_min_size: u64,
    /// Absolute-size trigger (0 = rule off).
    pub rewrite_bytes: u64,
    /// Staleness trigger, seconds (0 = rule off).
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

/// [`kevy_open`] with explicit options: durable at `dir` when `dir` is
/// non-null, in-memory when `dir` is null and `dir_len` is 0. A null
/// `opts` behaves exactly like `kevy_open` / `kevy_open_mem`. Returns
/// null on failure.
///
/// # Safety
/// `dir`, when non-null, must point to `dir_len` readable bytes; `opts`,
/// when non-null, must point to a readable [`KevyOpenOptions`].
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
        let bytes = unsafe { std::slice::from_raw_parts(dir, dir_len) };
        let Ok(path) = std::str::from_utf8(bytes) else {
            return std::ptr::null_mut();
        };
        Config::default().with_persist(path.to_owned())
    };
    let cfg = if opts.is_null() { base } else { apply(unsafe { &*opts }, base) };
    open_with(move || cfg)
}

/// Flush every shard's AOF with a REAL fsync, write the feed continuity
/// marker, then refuse every later write (reads stay available). The
/// deterministic teardown for a host's signal handler: `kevy_shutdown(db);
/// exit(0)`. Idempotent. Returns 0 on success, -1 on misuse, -2 on an
/// I/O failure (the store is still usable; retry or exit).
///
/// # Safety
/// `db` must be a live handle from `kevy_open*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_shutdown(db: *mut KevyDb) -> i32 {
    if db.is_null() {
        return -1;
    }
    let store = unsafe { &(*db).store };
    match catch_unwind(AssertUnwindSafe(|| store.shutdown())) {
        Ok(Ok(())) => 0,
        Ok(Err(_)) => -2,
        Err(_) => -2,
    }
}

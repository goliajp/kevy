//! Open-report over the C ABI — the machine-readable boot-replay verdict.
//!
//! `Store::open_report()`'s POD projection: counts, the corrupt flag, and
//! how many quarantine files the open's repair wrote (their paths sit next
//! to the AOF, named `aof-<id>.aof.corrupt-quarantine.<ts>`, and the boot
//! WARN line prints them). Split out of `lib.rs` for the 500-LOC house
//! rule; additive, `KEVY_ABI` unchanged.

use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::KevyDb;

/// What the open's replay restored — and what it could not. `dropped_bytes
/// > 0` or `corrupt != 0` means the store recovered LESS than its files
/// held (the dropped region was quarantined): surface it as a startup
/// health check.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KevyOpenReport {
    /// Commands replayed from the AOF(s), summed across shards.
    pub replayed_commands: u64,
    /// Bytes actually replayed (the valid prefixes).
    pub replayed_bytes: u64,
    /// Wall-clock time of the startup replay, in milliseconds.
    pub elapsed_ms: u64,
    /// Bytes dropped past the last replayable frame (quarantined).
    pub dropped_bytes: u64,
    /// 1 when any shard's replay stopped at a corrupt frame.
    pub corrupt: u8,
    /// Quarantine files written by the open's repair (one per affected
    /// shard).
    pub quarantine_count: u32,
}

/// Fill `*out` with the boot-replay verdict of `db`'s open. Returns 0 on
/// success, -1 on misuse (null handle/out).
///
/// # Safety
/// `db` must be a live handle from `kevy_open*`; `out` must point to a
/// writable [`KevyOpenReport`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_open_report(
    db: *mut KevyDb,
    out: *mut KevyOpenReport,
) -> i32 {
    if db.is_null() || out.is_null() {
        return -1;
    }
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
            unsafe { out.write(rep) };
            0
        }
        Err(_) => -1,
    }
}

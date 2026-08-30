//! Lifecycle, memory, and clock exports: everything the loader needs
//! before and around the data-plane calls.

use crate::{BAD_HANDLE, Instance, OK, REG, next_id, with};
use kevy_embedded::{Config, Store};

/// [`kevy_open`] flag: capture an AOF frame for every write so the host
/// can pump them into its own storage (see [`crate::abi_aof`]).
pub const OPEN_CAPTURE_AOF: u32 = 1;

/// The ABI contract version of this module. Loaders check it before any
/// other call and refuse a module whose version they don't know.
#[unsafe(no_mangle)]
pub extern "C" fn kevy_abi_version() -> u32 {
    crate::ABI_VERSION
}

/// Allocate `len` bytes of linear memory for the caller to stage
/// arguments in. Returns a pointer the caller must eventually hand back
/// to [`kevy_free`] with the same `len`. `len == 0` returns a dangling
/// (but well-aligned) pointer that is only valid to free.
#[unsafe(no_mangle)]
pub extern "C" fn kevy_alloc(len: u32) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len as usize);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// Release a buffer from [`kevy_alloc`]. `len` must be the size the
/// buffer was allocated with.
///
/// # Safety
///
/// `(ptr, len)` must be exactly a pair returned by / passed to
/// [`kevy_alloc`], freed at most once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_free(ptr: *mut u8, len: u32) {
    // SAFETY: contract above — reconstructing the Vec that `kevy_alloc`
    // forgot, with the original capacity, so drop releases it.
    unsafe { drop(Vec::from_raw_parts(ptr, 0, len as usize)) }
}

/// Open a store instance and return its handle (`0` on failure).
///
/// The instance is pure in-memory with the manual TTL reaper — the host
/// event loop drives expiry via [`kevy_tick`] and durability via the
/// AOF pump. `flags` is a bit set; see [`OPEN_CAPTURE_AOF`].
#[unsafe(no_mangle)]
pub extern "C" fn kevy_open(flags: u32) -> u32 {
    let Ok(store) = Store::open(Config::default().with_ttl_reaper_manual()) else {
        return 0;
    };
    let id = next_id();
    REG.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id, Instance::new(store, flags & OPEN_CAPTURE_AOF != 0));
    id
}

/// Close an instance: drops its subscriptions and the store. Undrained
/// AOF frames are discarded — pump them out first if they matter.
#[unsafe(no_mangle)]
pub extern "C" fn kevy_close(h: u32) -> i32 {
    match REG.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&h) {
        Some(_) => OK,
        None => BAD_HANDLE,
    }
}

/// Feed the engine clocks. Pass `Date.now()` (Unix-epoch milliseconds);
/// this drives both the monotonic TTL clock and the wall clock (absolute
/// expiry deadlines). Call before TTL-sensitive operations and once per
/// [`kevy_tick`]. The clock is module-wide (all instances share it). On
/// non-wasm targets the OS clock is authoritative and this is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn kevy_set_clock(now_ms: f64) {
    let ms = if now_ms > 0.0 { now_ms as u64 } else { 0 };
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        kevy_embedded::set_clock_ns(ms.saturating_mul(1_000_000));
        kevy_embedded::set_wall_clock_ms(ms);
    }
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    let _ = ms;
}

/// Run one TTL-reaper sweep; returns the number of keys expired (or
/// `-2` for a bad handle). Call ~10×/s, after [`kevy_set_clock`].
#[unsafe(no_mangle)]
pub extern "C" fn kevy_tick(h: u32) -> i32 {
    with(h, BAD_HANDLE, |inst| inst.store.tick().expired as i32)
}

/// Pointer to the instance's result buffer (null for a bad handle).
/// Valid until the next call on the same handle — copy out immediately.
#[unsafe(no_mangle)]
pub extern "C" fn kevy_out_ptr(h: u32) -> *const u8 {
    with(h, std::ptr::null(), |inst| inst.out.as_ptr())
}

/// Length of the instance's result buffer (0 for a bad handle).
#[unsafe(no_mangle)]
pub extern "C" fn kevy_out_len(h: u32) -> u32 {
    with(h, 0, |inst| inst.out.len() as u32)
}

//! Lifecycle, memory, and clock exports: everything the loader needs
//! before and around the data-plane calls.
//!
//! ```
//! use kevy_wasm::abi_core::*;
//! use kevy_wasm::abi_kv::*;
//! # fn out(h: u32) -> Vec<u8> {
//! #     // SAFETY: the result buffer stays valid until the next call on `h`.
//! #     unsafe { std::slice::from_raw_parts(kevy_out_ptr(h), kevy_out_len(h) as usize) }.to_vec()
//! # }
//! assert_eq!(kevy_abi_version(), 1);
//! let h = kevy_open(0);
//! let key = kevy_alloc(3); // stage the argument in module memory
//! // SAFETY: `key` is 3 writable bytes from `kevy_alloc`, freed once below.
//! unsafe {
//!     key.copy_from_nonoverlapping(b"hit".as_ptr(), 3);
//!     assert_eq!(kevy_incrby(h, key, 3, 1.0), 0);
//!     kevy_free(key, 3);
//! }
//! assert_eq!(out(h), b"1");
//! assert_eq!(kevy_close(h), 0);
//! ```

use crate::{BAD_HANDLE, Instance, OK, REG, next_id, with};
use kevy_embedded::{Config, Store};

/// [`kevy_open`] flag: capture an AOF frame for every write so the host
/// can pump them into its own storage (see [`crate::abi_aof`]).
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// use kevy_wasm::abi_aof::kevy_aof_frames_out;
///
/// let (plain, logged) = (kevy_open(0), kevy_open(OPEN_CAPTURE_AOF));
/// for h in [plain, logged] {
///     // SAFETY: each pair points at that many readable bytes for the call.
///     unsafe { kevy_set(h, b"k".as_ptr(), 1, b"v".as_ptr(), 1) };
/// }
/// assert_eq!(kevy_aof_frames_out(plain), 0); // nothing captured
/// assert!(kevy_aof_frames_out(logged) > 0); // the SET, as an AOF frame
/// kevy_close(plain);
/// kevy_close(logged);
/// ```
pub const OPEN_CAPTURE_AOF: u32 = 1;

/// The ABI contract version of this module. Loaders check it before any
/// other call and refuse a module whose version they don't know.
///
/// ```
/// use kevy_wasm::abi_core::*;
/// const KNOWN: u32 = 1;
/// assert_eq!(kevy_abi_version(), KNOWN);
/// ```
#[unsafe(no_mangle)]
pub extern "C" fn kevy_abi_version() -> u32 {
    crate::ABI_VERSION
}

/// Allocate `len` bytes of linear memory for the caller to stage
/// arguments in. Returns a pointer the caller must eventually hand back
/// to [`kevy_free`] with the same `len`. `len == 0` returns a dangling
/// (but well-aligned) pointer that is only valid to free.
///
/// ```
/// use kevy_wasm::abi_core::*;
/// let p = kevy_alloc(4);
/// // SAFETY: `p` holds 4 writable bytes until the matching free.
/// unsafe {
///     p.copy_from_nonoverlapping(b"ping".as_ptr(), 4);
///     assert_eq!(std::slice::from_raw_parts(p, 4), b"ping");
///     kevy_free(p, 4);
///     kevy_free(kevy_alloc(0), 0); // the empty allocation is freed too
/// }
/// ```
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
/// ```
/// use kevy_wasm::abi_core::*;
/// let p = kevy_alloc(16);
/// // SAFETY: the exact pair `kevy_alloc` returned, freed once.
/// unsafe { kevy_free(p, 16) };
/// ```
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
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// let (a, b) = (kevy_open(0), kevy_open(0));
/// assert!(a != 0 && b != 0 && a != b); // independent instances
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe { kevy_set(a, b"k".as_ptr(), 1, b"v".as_ptr(), 1) };
/// assert_eq!((kevy_dbsize(a), kevy_dbsize(b)), (1.0, 0.0));
/// kevy_close(a);
/// kevy_close(b);
/// ```
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
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// let h = kevy_open(0);
/// assert_eq!(kevy_close(h), 0);
/// assert_eq!(kevy_close(h), -2); // the handle is gone
/// assert!(kevy_dbsize(h).is_nan());
/// ```
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
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// let h = kevy_open(0);
/// let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis();
/// kevy_set_clock(now_ms as f64); // what a browser host does with `Date.now()`
/// // SAFETY: each pair points at that many readable bytes for the call.
/// let left = unsafe {
///     kevy_set_ttl(h, b"k".as_ptr(), 1, b"v".as_ptr(), 1, 60_000.0);
///     kevy_pttl(h, b"k".as_ptr(), 1)
/// };
/// assert!(left > 0.0 && left <= 60_000.0);
/// kevy_close(h);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
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
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe { kevy_set_ttl(h, b"k".as_ptr(), 1, b"v".as_ptr(), 1, 1.0) };
/// std::thread::sleep(std::time::Duration::from_millis(20));
/// // the host's timer drives expiry; each sweep samples part of the keyspace
/// let expired: i32 = (0..100).map(|_| kevy_tick(h)).sum();
/// assert_eq!(expired, 1);
/// assert_eq!(kevy_dbsize(h), 0.0);
/// assert_eq!(kevy_tick(0), -2);
/// kevy_close(h);
/// ```
#[unsafe(no_mangle)]
pub extern "C" fn kevy_tick(h: u32) -> i32 {
    with(h, BAD_HANDLE, |inst| inst.store.tick().expired as i32)
}

/// Pointer to the instance's result buffer (null for a bad handle).
/// Valid until the next call on the same handle — copy out immediately.
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe { kevy_incrby(h, b"n".as_ptr(), 1, 42.0) };
/// // SAFETY: the result buffer is valid until the next call on `h`.
/// let copied = unsafe { std::slice::from_raw_parts(kevy_out_ptr(h), kevy_out_len(h) as usize) }.to_vec();
/// assert_eq!(copied, b"42");
/// assert!(kevy_out_ptr(0).is_null());
/// kevy_close(h);
/// ```
#[unsafe(no_mangle)]
pub extern "C" fn kevy_out_ptr(h: u32) -> *const u8 {
    with(h, std::ptr::null(), |inst| inst.out.as_ptr())
}

/// Length of the instance's result buffer (0 for a bad handle).
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe { kevy_set(h, b"k".as_ptr(), 1, b"abc".as_ptr(), 3) };
/// // SAFETY: as above.
/// assert_eq!(unsafe { kevy_get(h, b"k".as_ptr(), 1) }, 1);
/// assert_eq!(kevy_out_len(h), 3);
/// assert_eq!(kevy_out_len(0), 0);
/// kevy_close(h);
/// ```
#[unsafe(no_mangle)]
pub extern "C" fn kevy_out_len(h: u32) -> u32 {
    with(h, 0, |inst| inst.out.len() as u32)
}

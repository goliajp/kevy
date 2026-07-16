//! C ABI for embedding kevy.
//!
//! One design decision carries this whole crate: there is **no per-verb C
//! function**. `kevy_cmd` takes argv and returns the RESP-encoded reply —
//! the same path the embedded RESP listener serves — so all 184 verbs are
//! reachable through one symbol, and a new verb needs zero ABI change.
//! Language bindings pair it with a ~150-line RESP parser; RESP is the one
//! encoding every Redis-adjacent ecosystem already speaks.
//!
//! Pub/sub is **polled**, not called back: a callback crossing the FFI on
//! the publisher's thread is a reentrancy and GC-interop hazard in Go and
//! C# (and the wasm binding already established the pump model). The
//! subscriber drains frames with `kevy_sub_next`, each frame encoded as the
//! same RESP array the server would push.
//!
//! Every entry point catches panics: unwinding across an `extern "C"`
//! boundary is undefined behaviour, and this is a trust boundary — the
//! caller may be any language runtime.

use std::panic::{AssertUnwindSafe, catch_unwind};

use kevy_embedded::{Config, KevyError, Store, Subscription};

mod frame;
use frame::encode_frame;

mod dispatch;
mod publish;
mod sub_raw;
pub use dispatch::dispatch_packed;
pub use publish::kevy_publish;
pub use sub_raw::{kevy_sub_next_raw, kevy_sub_wait_raw};

/// Opaque database handle. A `Box<Store>` on the Rust side.
pub struct KevyDb {
    store: Store,
}

/// Opaque subscription handle. A `Box<Subscription>` on the Rust side.
pub struct KevySub {
    pub(crate) sub: Subscription,
}

/// A byte buffer owned by kevy, returned to the caller. Free it with
/// [`kevy_buf_free`]. `ptr` is null only for a miss/error; a present empty
/// value has a non-null (dangling) `ptr` with `len == 0`.
#[repr(C)]
pub struct KevyBuf {
    /// Start of the buffer (allocated by Rust; never free() it).
    pub ptr: *mut u8,
    /// Length in bytes.
    pub len: usize,
    /// Capacity — carried so the Vec can be rebuilt exactly on free.
    pub cap: usize,
}

impl KevyBuf {
    pub(crate) fn from_vec(v: Vec<u8>) -> Self {
        let mut v = std::mem::ManuallyDrop::new(v);
        Self { ptr: v.as_mut_ptr(), len: v.len(), cap: v.capacity() }
    }

    pub(crate) const fn empty() -> Self {
        Self { ptr: std::ptr::null_mut(), len: 0, cap: 0 }
    }
}

/// ABI version. Bump only on a breaking change to these signatures.
pub const KEVY_ABI: u32 = 1;

/// Returns the ABI version ([`KEVY_ABI`]).
#[unsafe(no_mangle)]
pub extern "C" fn kevy_abi() -> u32 {
    KEVY_ABI
}

/// Returns the engine version as a static NUL-terminated string.
#[unsafe(no_mangle)]
pub extern "C" fn kevy_version() -> *const std::ffi::c_char {
    static V: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");
    V.as_ptr().cast()
}

/// Open a persistent store rooted at `dir` (UTF-8, `dir_len` bytes, not
/// NUL-terminated). Returns null on failure — invalid UTF-8, or the
/// directory could not be created/replayed.
///
/// # Safety
/// `dir` must point to `dir_len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_open(dir: *const u8, dir_len: usize) -> *mut KevyDb {
    if dir.is_null() {
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(dir, dir_len) };
    let Ok(path) = std::str::from_utf8(bytes) else {
        return std::ptr::null_mut();
    };
    let path = path.to_owned();
    open_with(move || Config::default().with_persist(path))
}

/// Open a pure in-memory store: no directory, nothing survives the process.
#[unsafe(no_mangle)]
pub extern "C" fn kevy_open_mem() -> *mut KevyDb {
    open_with(Config::default)
}

fn open_with(cfg: impl FnOnce() -> Config) -> *mut KevyDb {
    let opened = catch_unwind(AssertUnwindSafe(|| Store::open(cfg())));
    match opened {
        Ok(Ok(store)) => Box::into_raw(Box::new(KevyDb { store })),
        _ => std::ptr::null_mut(),
    }
}

/// Close a store and release everything it holds. `db` must come from
/// [`kevy_open`] / [`kevy_open_mem`] and must not be used afterwards.
/// Null is a no-op.
///
/// # Safety
/// `db` must be a live handle from this library, passed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_close(db: *mut KevyDb) {
    if db.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| drop(unsafe { Box::from_raw(db) })));
}

/// Execute one command. `argv` is `argc` pointers with lengths in
/// `argv_len`; the RESP-encoded reply is written to `out`.
///
/// Returns 0 on success — a protocol-level error (`-ERR …`) is still a
/// *successful* call with a RESP error in `out`. Non-zero means the call
/// itself was misused (null handle/args, zero argc, or an internal panic);
/// `out` is then empty and must not be freed.
///
/// # Safety
/// All pointers must be valid for the lengths given; `out` must point to
/// writable [`KevyBuf`] storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_cmd(
    db: *mut KevyDb,
    argc: usize,
    argv: *const *const u8,
    argv_len: *const usize,
    out: *mut KevyBuf,
) -> i32 {
    if out.is_null() {
        return -1;
    }
    unsafe { out.write(KevyBuf::empty()) };
    if db.is_null() || argc == 0 || argv.is_null() || argv_len.is_null() {
        return -1;
    }
    let store = unsafe { &(*db).store };
    let ptrs = unsafe { std::slice::from_raw_parts(argv, argc) };
    let lens = unsafe { std::slice::from_raw_parts(argv_len, argc) };
    if ptrs.iter().any(|p| p.is_null()) {
        return -1;
    }
    let args: Vec<Vec<u8>> = ptrs
        .iter()
        .zip(lens)
        .map(|(&p, &l)| unsafe { std::slice::from_raw_parts(p, l) }.to_vec())
        .collect();
    let reply = catch_unwind(AssertUnwindSafe(|| {
        let mut buf = Vec::new();
        store.dispatch_argv(&args, &mut buf);
        buf
    }));
    match reply {
        Ok(buf) => {
            unsafe { out.write(KevyBuf::from_vec(buf)) };
            0
        }
        Err(_) => -2,
    }
}

/// Free a buffer returned by this library — pass the three fields of the
/// [`KevyBuf`] unchanged. Scalars rather than the struct by value on
/// purpose: a >16-byte struct parameter is passed indirectly on AArch64,
/// which half the FFI toolchains (bun:ffi among them) cannot express.
/// A null `ptr` is a no-op.
///
/// # Safety
/// The triple must be exactly as returned, freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_buf_free(ptr: *mut u8, len: usize, cap: usize) {
    if ptr.is_null() {
        return;
    }
    drop(unsafe { Vec::from_raw_parts(ptr, len, cap) });
}

/// Scalar fast path: `GET` without argv assembly or RESP encoding — the
/// raw value bytes land in `out`. Returns 1 on hit, 0 on miss, negative on
/// misuse. This is the lane the mobile bindings' hot path lives on, where
/// the bar is an mmap KV's synchronous read.
///
/// # Safety
/// `key` must point to `key_len` readable bytes; `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_get(
    db: *mut KevyDb,
    key: *const u8,
    key_len: usize,
    out: *mut KevyBuf,
) -> i32 {
    if out.is_null() {
        return -1;
    }
    unsafe { out.write(KevyBuf::empty()) };
    if db.is_null() || key.is_null() {
        return -1;
    }
    let store = unsafe { &(*db).store };
    let k = unsafe { std::slice::from_raw_parts(key, key_len) };
    match catch_unwind(AssertUnwindSafe(|| store.get(k))) {
        Ok(Ok(Some(v))) => {
            unsafe { out.write(KevyBuf::from_vec(v)) };
            1
        }
        Ok(Ok(None)) => 0,
        _ => -2,
    }
}

/// Scalar GET, **zero-copy shared lane**. For a bulk value the engine's
/// `Arc<Box<[u8]>>` is cloned (a refcount bump, no byte copy) and handed out as
/// a buffer that VIEWS the Arc's bytes — the analog of MMKV returning a view of
/// its mmap page; small values get a plain owned Vec (one alloc).
/// In the returned `KevyBuf`, `ptr`+`len` are the value view and `cap` is an
/// OPAQUE owner handle. Free ONLY with [`kevy_buf_free_shared`] — never
/// [`kevy_buf_free`]. 1 = hit, 0 = miss, negative = misuse.
///
/// # Safety
/// `key` must point to `key_len` readable bytes; `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_get_shared(
    db: *mut KevyDb,
    key: *const u8,
    key_len: usize,
    out: *mut KevyBuf,
) -> i32 {
    if out.is_null() {
        return -1;
    }
    unsafe { out.write(KevyBuf::empty()) };
    if db.is_null() || key.is_null() {
        return -1;
    }
    let store = unsafe { &(*db).store };
    let k = unsafe { std::slice::from_raw_parts(key, key_len) };
    match catch_unwind(AssertUnwindSafe(|| store.get_shared_owned(k))) {
        Ok(Ok(Some(shared))) => {
            // `cap` doubles as a tagged owner handle so the shared free knows
            // how to reclaim: low bit 0 = an Arc raw pointer (bulk, always
            // 8-aligned so the bit is free); low bit 1 = a Vec (small), with
            // its capacity in the high bits. Bulk is zero-copy; small is a
            // single-alloc Vec (never the extra fresh-Arc allocation).
            let (data, len, cap) = match shared {
                kevy_embedded::GetShared::Arc(arc) => {
                    // Read view ptr/len before into_raw (deref coercion
                    // Arc<Box<[u8]>> -> [u8]; no raw-pointer autoref).
                    let slice: &[u8] = &arc;
                    let d = slice.as_ptr() as *mut u8;
                    let l = slice.len();
                    let raw = std::sync::Arc::into_raw(arc); // 8-aligned → tag 0
                    (d, l, raw as usize)
                }
                kevy_embedded::GetShared::Bytes(v) => {
                    let mut v = std::mem::ManuallyDrop::new(v);
                    (v.as_mut_ptr(), v.len(), (v.capacity() << 1) | 1)
                }
            };
            unsafe { out.write(KevyBuf { ptr: data, len, cap }) };
            1
        }
        Ok(Ok(None)) => 0,
        _ => -2,
    }
}

/// Free a buffer returned by [`kevy_get_shared`] — drops the engine `Arc`.
/// `ptr`/`len` are ignored; `cap` is the opaque owner handle from the shared
/// GET. Pairs 1:1 with [`kevy_get_shared`]; do NOT mix with [`kevy_buf_free`].
///
/// # Safety
/// `cap` must be a value produced by [`kevy_get_shared`], freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_buf_free_shared(ptr: *mut u8, len: usize, cap: usize) {
    if cap == 0 {
        return; // empty sentinel
    }
    if cap & 1 == 1 {
        // Vec-backed small value: capacity in the high bits.
        drop(unsafe { Vec::from_raw_parts(ptr, len, cap >> 1) });
    } else {
        // Arc-backed bulk value: cap is the Arc raw pointer.
        drop(unsafe { std::sync::Arc::from_raw(cap as *const Box<[u8]>) });
    }
}

/// Scalar fast path: `SET`, optionally with a TTL (`ttl_ms` 0 = none).
/// Returns 0 on success, negative on misuse or a storage error.
///
/// # Safety
/// `key` / `val` must point to their given lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_set(
    db: *mut KevyDb,
    key: *const u8,
    key_len: usize,
    val: *const u8,
    val_len: usize,
    ttl_ms: u64,
) -> i32 {
    if db.is_null() || key.is_null() || val.is_null() {
        return -1;
    }
    let store = unsafe { &(*db).store };
    let k = unsafe { std::slice::from_raw_parts(key, key_len) };
    let v = unsafe { std::slice::from_raw_parts(val, val_len) };
    let done = catch_unwind(AssertUnwindSafe(|| {
        if ttl_ms == 0 {
            store.set(k, v).map(|_| ())
        } else {
            store
                .set_with_ttl(k, v, std::time::Duration::from_millis(ttl_ms))
                .map(|_| ())
        }
    }));
    match done {
        Ok(Ok(())) => 0,
        _ => -2,
    }
}

/// Open a subscription on one channel (call again for more channels — or
/// subscribe to a pattern with [`kevy_psubscribe`]). Returns null on error.
///
/// # Safety
/// `chan` must point to `chan_len` readable bytes; `db` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_subscribe(
    db: *mut KevyDb,
    chan: *const u8,
    chan_len: usize,
) -> *mut KevySub {
    unsafe { sub_open(db, chan, chan_len, false) }
}

/// Open a subscription on one glob pattern (`room:*`). Returns null on error.
///
/// # Safety
/// Same contract as [`kevy_subscribe`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_psubscribe(
    db: *mut KevyDb,
    pat: *const u8,
    pat_len: usize,
) -> *mut KevySub {
    unsafe { sub_open(db, pat, pat_len, true) }
}

unsafe fn sub_open(
    db: *mut KevyDb,
    chan: *const u8,
    chan_len: usize,
    pattern: bool,
) -> *mut KevySub {
    if db.is_null() || chan.is_null() {
        return std::ptr::null_mut();
    }
    let store = unsafe { &(*db).store };
    let name = unsafe { std::slice::from_raw_parts(chan, chan_len) };
    let opened = catch_unwind(AssertUnwindSafe(|| {
        if pattern { store.psubscribe(&[name]) } else { store.subscribe(&[name]) }
    }));
    match opened {
        Ok(sub) => Box::into_raw(Box::new(KevySub { sub })),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Drain one pending pub/sub frame without blocking.
///
/// Returns 1 with `out` holding the frame encoded as the RESP array the
/// server would push (`*3 … message <channel> <payload>`, `*4 … pmessage
/// <pattern> <channel> <payload>`, and the subscribe/unsubscribe acks);
/// 0 when nothing is queued; negative on misuse.
///
/// # Safety
/// `sub` must be live; `out` must point to writable [`KevyBuf`] storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_sub_next(sub: *mut KevySub, out: *mut KevyBuf) -> i32 {
    if out.is_null() {
        return -1;
    }
    unsafe { out.write(KevyBuf::empty()) };
    if sub.is_null() {
        return -1;
    }
    let s = unsafe { &(*sub).sub };
    let polled = catch_unwind(AssertUnwindSafe(|| s.try_recv()));
    match polled {
        Ok(Ok(Some(frame))) => {
            unsafe { out.write(KevyBuf::from_vec(encode_frame(&frame))) };
            1
        }
        Ok(Ok(None)) => 0,
        _ => -2,
    }
}

/// Block until one pub/sub frame is queued, or `timeout_ms` elapses.
///
/// Same output/return shape as [`kevy_sub_next`], but instead of returning
/// 0 the instant the queue is empty it **parks the calling thread** (no
/// busy-poll) up to `timeout_ms`: 1 with `out` holding the frame, 0 on
/// timeout, negative on misuse or once the bus tears down. `timeout_ms == 0`
/// waits indefinitely (until a frame arrives or the bus is gone).
///
/// The engine already parks efficiently on its `mpsc` channel; this just
/// exposes that wait to the C ABI so a push-style binding (a poller thread
/// that hops each frame onto a host runtime) can block in the kernel
/// instead of spinning `kevy_sub_next` and burning a core.
///
/// # Safety
/// `sub` must be live; `out` must point to writable [`KevyBuf`] storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_sub_wait(sub: *mut KevySub, timeout_ms: u64, out: *mut KevyBuf) -> i32 {
    if out.is_null() {
        return -1;
    }
    unsafe { out.write(KevyBuf::empty()) };
    if sub.is_null() {
        return -1;
    }
    let s = unsafe { &(*sub).sub };
    let waited = catch_unwind(AssertUnwindSafe(|| {
        if timeout_ms == 0 {
            s.recv().map(Some)
        } else {
            match s.recv_timeout(std::time::Duration::from_millis(timeout_ms)) {
                Ok(f) => Ok(Some(f)),
                Err(KevyError::TimedOut) => Ok(None),
                Err(e) => Err(e),
            }
        }
    }));
    match waited {
        Ok(Ok(Some(frame))) => {
            unsafe { out.write(KevyBuf::from_vec(encode_frame(&frame))) };
            1
        }
        Ok(Ok(None)) => 0, // timeout, nothing arrived
        _ => -2,           // bus closed or panic
    }
}

/// Close a subscription (unsubscribes from everything it held). Null is a
/// no-op.
///
/// # Safety
/// `sub` must be a live handle from this library, passed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_sub_close(sub: *mut KevySub) {
    if sub.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| drop(unsafe { Box::from_raw(sub) })));
}

/// Decode the packed argv the byte-array-oriented bindings send (JNI and
/// N-API both speak it): each argument is a u32-LE length prefix followed
/// by that many bytes, back to back. `None` on a truncated prefix/body or
/// zero arguments — misuse, not a protocol error.
///
/// This is a Rust-side helper for the binding shells, not part of the C ABI.
pub fn unpack_argv(packed: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut args = Vec::new();
    let mut pos = 0usize;
    while pos < packed.len() {
        let head = packed.get(pos..pos + 4)?;
        let len = u32::from_le_bytes(head.try_into().ok()?) as usize;
        pos += 4;
        let body = packed.get(pos..pos + len)?;
        args.push(body.to_vec());
        pos += len;
    }
    if args.is_empty() { None } else { Some(args) }
}

#[cfg(test)]
mod abi_tests;

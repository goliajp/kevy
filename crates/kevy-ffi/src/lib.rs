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
//!
//! ```
//! use kevy_ffi::{KevyBuf, kevy_buf_free, kevy_close, kevy_cmd, kevy_open_mem};
//!
//! let db = kevy_open_mem();
//! let argv: [&[u8]; 3] = [b"SET", b"greeting", b"hello"];
//! let ptrs: Vec<*const u8> = argv.iter().map(|a| a.as_ptr()).collect();
//! let lens: Vec<usize> = argv.iter().map(|a| a.len()).collect();
//! let mut out = KevyBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
//! // SAFETY: `db` is live; `ptrs`/`lens` hold 3 entries pointing into `argv`;
//! // the reply is read before its single free and `db` is closed once.
//! unsafe {
//!     assert_eq!(kevy_cmd(db, 3, ptrs.as_ptr(), lens.as_ptr(), &mut out), 0);
//!     assert_eq!(std::slice::from_raw_parts(out.ptr, out.len), b"+OK\r\n");
//!     kevy_buf_free(out.ptr, out.len, out.cap);
//!     kevy_close(db);
//! }
//! ```

// `catch_unwind` at the ABI boundary. Its `Err` is the panic payload,
// and the point of catching it here is that a panic must not cross
// into C — see `boundary/no-panic-across-abi`. There is no Rust
// frame above this to hand it to, and the callee has already
// reported through its own error channel.
#![expect(
    clippy::let_underscore_must_use,
    reason = "catch_unwind exists to stop the unwind, not to report it"
)]

use std::panic::{AssertUnwindSafe, catch_unwind};

use kevy_embedded::{Config, Store, Subscription};
mod batch;
mod dispatch;
mod frame;
mod lifecycle;
mod publish;
mod report;
mod scalar;
mod sub;
mod sub_raw;
pub use dispatch::{MGET_MISS, dispatch_packed, get_lent, mget_packed, mset_packed};
pub use lifecycle::{KevyOpenOptions, kevy_open_with, kevy_shutdown};
pub use publish::kevy_publish;
pub use report::{KevyOpenReport, kevy_open_report};
pub use scalar::{kevy_buf_free_shared, kevy_get, kevy_get_shared, kevy_set};
pub use sub::{kevy_psubscribe, kevy_sub_close, kevy_sub_next, kevy_sub_wait, kevy_subscribe};
pub use sub_raw::{kevy_sub_next_raw, kevy_sub_wait_raw};

/// Opaque database handle. A `Box<Store>` on the Rust side.
///
/// ```
/// let db: *mut kevy_ffi::KevyDb = kevy_ffi::kevy_open_mem();
/// assert!(!db.is_null());
/// // SAFETY: the handle just opened, closed exactly once.
/// unsafe { kevy_ffi::kevy_close(db) };
/// ```
pub struct KevyDb {
    pub(crate) store: Store,
}

/// Opaque subscription handle. A `Box<Subscription>` on the Rust side.
///
/// ```
/// use kevy_ffi::{KevySub, kevy_close, kevy_open_mem, kevy_sub_close, kevy_subscribe};
///
/// let db = kevy_open_mem();
/// // SAFETY: `db` is live and the channel name is 4 readable bytes; each
/// // handle is closed exactly once, the subscription first.
/// unsafe {
///     let sub: *mut KevySub = kevy_subscribe(db, b"news".as_ptr(), 4);
///     assert!(!sub.is_null());
///     kevy_sub_close(sub);
///     kevy_close(db);
/// }
/// ```
pub struct KevySub {
    pub(crate) sub: Subscription,
}

/// A byte buffer owned by kevy, returned to the caller. Free it with
/// [`kevy_buf_free`]. `ptr` is null only for a miss/error; a present empty
/// value has a non-null (dangling) `ptr` with `len == 0`.
///
/// ```
/// use kevy_ffi::{KevyBuf, kevy_buf_free, kevy_close, kevy_get, kevy_open_mem, kevy_set};
///
/// let db = kevy_open_mem();
/// let mut out = KevyBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
/// // SAFETY: `db` is live; every key/value pointer covers its length; the
/// // buffer is freed once with its triple unchanged; `db` is closed once.
/// unsafe {
///     assert_eq!(kevy_set(db, b"k".as_ptr(), 1, b"".as_ptr(), 0, 0), 0);
///     assert_eq!(kevy_get(db, b"k".as_ptr(), 1, &mut out), 1);
///     assert!(!out.ptr.is_null() && out.len == 0); // present but empty, not a miss
///     kevy_buf_free(out.ptr, out.len, out.cap);
///     kevy_close(db);
/// }
/// ```
#[repr(C)]
pub struct KevyBuf {
    /// Start of the buffer (allocated by Rust; never free() it).
    ///
    /// ```
    /// use kevy_ffi::{KevyBuf, kevy_buf_free, kevy_close, kevy_get, kevy_open_mem, kevy_set};
    ///
    /// let db = kevy_open_mem();
    /// let mut out = KevyBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
    /// // SAFETY: `db` is live; every key/value pointer covers its length; the
    /// // buffer is read before its single free; `db` is closed once.
    /// unsafe {
    ///     assert_eq!(kevy_set(db, b"k".as_ptr(), 1, b"value".as_ptr(), 5, 0), 0);
    ///     assert_eq!(kevy_get(db, b"k".as_ptr(), 1, &mut out), 1);
    ///     assert_eq!(std::slice::from_raw_parts(out.ptr, out.len), b"value");
    ///     kevy_buf_free(out.ptr, out.len, out.cap);
    ///     kevy_close(db);
    /// }
    /// ```
    pub ptr: *mut u8,
    /// Length in bytes.
    ///
    /// ```
    /// use kevy_ffi::{KevyBuf, kevy_buf_free, kevy_close, kevy_get, kevy_open_mem, kevy_set};
    ///
    /// let db = kevy_open_mem();
    /// let mut out = KevyBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
    /// // SAFETY: `db` is live; every key/value pointer covers its length; the
    /// // buffer is read before its single free; `db` is closed once.
    /// unsafe {
    ///     assert_eq!(kevy_set(db, b"k".as_ptr(), 1, b"value".as_ptr(), 5, 0), 0);
    ///     assert_eq!(kevy_get(db, b"k".as_ptr(), 1, &mut out), 1);
    ///     assert_eq!(out.len, 5);
    ///     kevy_buf_free(out.ptr, out.len, out.cap);
    ///     kevy_close(db);
    /// }
    /// ```
    pub len: usize,
    /// Capacity — carried so the Vec can be rebuilt exactly on free.
    ///
    /// ```
    /// use kevy_ffi::{KevyBuf, kevy_buf_free, kevy_close, kevy_get, kevy_open_mem, kevy_set};
    ///
    /// let db = kevy_open_mem();
    /// let mut out = KevyBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
    /// // SAFETY: `db` is live; every key/value pointer covers its length; the
    /// // buffer is read before its single free; `db` is closed once.
    /// unsafe {
    ///     assert_eq!(kevy_set(db, b"k".as_ptr(), 1, b"value".as_ptr(), 5, 0), 0);
    ///     assert_eq!(kevy_get(db, b"k".as_ptr(), 1, &mut out), 1);
    ///     assert!(out.cap >= out.len); // handed back unchanged to the free
    ///     kevy_buf_free(out.ptr, out.len, out.cap);
    ///     kevy_close(db);
    /// }
    /// ```
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
///
/// ```
/// // a binding refuses a library whose ABI it was not written against
/// assert_eq!(kevy_ffi::kevy_abi(), kevy_ffi::KEVY_ABI);
/// ```
pub const KEVY_ABI: u32 = 1;

/// Returns the ABI version ([`KEVY_ABI`]).
///
/// ```
/// assert_eq!(kevy_ffi::kevy_abi(), 1);
/// ```
// NO-UNWIND: returns a constant
#[unsafe(no_mangle)]
pub extern "C" fn kevy_abi() -> u32 {
    KEVY_ABI
}

/// Returns the engine version as a static NUL-terminated string.
///
/// ```
/// // SAFETY: the pointer is to a static NUL-terminated string.
/// let v = unsafe { std::ffi::CStr::from_ptr(kevy_ffi::kevy_version()) };
/// assert_eq!(v.to_str()?, env!("CARGO_PKG_VERSION"));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
// NO-UNWIND: returns a pointer to a static string
#[unsafe(no_mangle)]
pub extern "C" fn kevy_version() -> *const std::ffi::c_char {
    static V: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");
    V.as_ptr().cast()
}

/// Open a persistent store rooted at `dir` (UTF-8, `dir_len` bytes, not
/// NUL-terminated). Returns null on failure — invalid UTF-8, or the
/// directory could not be created/replayed.
///
/// ```
/// use kevy_ffi::{KevyBuf, kevy_buf_free, kevy_close, kevy_get, kevy_open, kevy_set};
///
/// let dir = std::env::temp_dir().join(format!("kevy-ffi-open-doc-{}", std::process::id()));
/// let path = dir.to_str().ok_or("temp dir is not UTF-8")?;
/// // SAFETY: `path` is `path.len()` readable bytes; every key/value pointer
/// // covers its length; the buffer is freed once; each handle is closed once.
/// unsafe {
///     let db = kevy_open(path.as_ptr(), path.len());
///     assert!(!db.is_null());
///     assert_eq!(kevy_set(db, b"k".as_ptr(), 1, b"kept".as_ptr(), 4, 0), 0);
///     kevy_close(db);
///
///     let db = kevy_open(path.as_ptr(), path.len()); // replays the log
///     let mut out = KevyBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
///     assert_eq!(kevy_get(db, b"k".as_ptr(), 1, &mut out), 1);
///     assert_eq!(std::slice::from_raw_parts(out.ptr, out.len), b"kept");
///     kevy_buf_free(out.ptr, out.len, out.cap);
///     kevy_close(db);
/// }
/// std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Safety
/// `dir` must point to `dir_len` readable bytes.
// NO-UNWIND: the store is opened inside open_with, which catches; the rest is a UTF-8 check
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_open(dir: *const u8, dir_len: usize) -> *mut KevyDb {
    if dir.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the `# Safety` contract above covers this pointer/length pair.
    let bytes = unsafe { std::slice::from_raw_parts(dir, dir_len) };
    let Ok(path) = std::str::from_utf8(bytes) else {
        return std::ptr::null_mut();
    };
    let path = path.to_owned();
    open_with(move || Config::default().with_persist(path))
}

/// Open a pure in-memory store: no directory, nothing survives the process.
///
/// ```
/// use kevy_ffi::{KevyBuf, kevy_close, kevy_get, kevy_open_mem};
///
/// let db = kevy_open_mem();
/// let mut out = KevyBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
/// // SAFETY: `db` is live; the key pointer covers its length; closed once.
/// unsafe {
///     assert_eq!(kevy_get(db, b"k".as_ptr(), 1, &mut out), 0); // starts empty
///     kevy_close(db);
/// }
/// ```
// NO-UNWIND: the store is opened inside open_with, which catches
#[unsafe(no_mangle)]
pub extern "C" fn kevy_open_mem() -> *mut KevyDb {
    open_with(Config::default)
}

pub(crate) fn open_with(cfg: impl FnOnce() -> Config) -> *mut KevyDb {
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
/// ```
/// let db = kevy_ffi::kevy_open_mem();
/// // SAFETY: a live handle, passed exactly once; null is a no-op.
/// unsafe {
///     kevy_ffi::kevy_close(db);
///     kevy_ffi::kevy_close(std::ptr::null_mut());
/// }
/// ```
///
/// # Safety
/// `db` must be a live handle from this library, passed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_close(db: *mut KevyDb) {
    if db.is_null() {
        return;
    }
    // SAFETY: the contract makes the caller pass a handle this crate produced with
    // `Box::into_raw` and never freed, so this takes ownership back exactly once.
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
/// ```
/// use kevy_ffi::{KevyBuf, kevy_buf_free, kevy_close, kevy_cmd, kevy_open_mem};
///
/// let db = kevy_open_mem();
/// let argv: [&[u8]; 2] = [b"INCR", b"hits"];
/// let ptrs: Vec<*const u8> = argv.iter().map(|a| a.as_ptr()).collect();
/// let lens: Vec<usize> = argv.iter().map(|a| a.len()).collect();
/// let mut out = KevyBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
/// // SAFETY: `db` is live; `ptrs`/`lens` hold 2 entries pointing into `argv`;
/// // the reply is read before its single free; `db` is closed once.
/// unsafe {
///     assert_eq!(kevy_cmd(db, 2, ptrs.as_ptr(), lens.as_ptr(), &mut out), 0);
///     assert_eq!(std::slice::from_raw_parts(out.ptr, out.len), b":1\r\n");
///     kevy_buf_free(out.ptr, out.len, out.cap);
///     // misuse: zero arguments; `out` is left empty and is not freed
///     assert_eq!(kevy_cmd(db, 0, ptrs.as_ptr(), lens.as_ptr(), &mut out), -1);
///     assert!(out.ptr.is_null());
///     kevy_close(db);
/// }
/// ```
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
    // SAFETY: covered by this fn's `# Safety` contract, with the null case ruled out by
    // the checks above.
    unsafe { out.write(KevyBuf::empty()) };
    if db.is_null() || argc == 0 || argv.is_null() || argv_len.is_null() {
        return -1;
    }
    // SAFETY: checked non-null above; the contract requires a live `kevy_open*` handle.
    let store = unsafe { &(*db).store };
    // SAFETY: the `# Safety` contract above covers this pointer/length pair.
    let ptrs = unsafe { std::slice::from_raw_parts(argv, argc) };
    // SAFETY: the `# Safety` contract above covers this pointer/length pair.
    let lens = unsafe { std::slice::from_raw_parts(argv_len, argc) };
    if ptrs.iter().any(|p| p.is_null()) {
        return -1;
    }
    let args: Vec<Vec<u8>> = ptrs
        .iter()
        .zip(lens)
        // SAFETY: the `# Safety` contract above covers this pointer/length pair.
        .map(|(&p, &l)| unsafe { std::slice::from_raw_parts(p, l) }.to_vec())
        .collect();
    let reply = catch_unwind(AssertUnwindSafe(|| {
        let mut buf = Vec::new();
        store.dispatch_argv(&args, &mut buf);
        buf
    }));
    match reply {
        Ok(buf) => {
            // SAFETY: covered by this fn's `# Safety` contract, with the null case ruled out by
            // the checks above.
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
/// ```
/// use kevy_ffi::{KevyBuf, kevy_buf_free, kevy_close, kevy_get, kevy_open_mem, kevy_set};
///
/// let db = kevy_open_mem();
/// let mut out = KevyBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
/// // SAFETY: `db` is live; the buffer is freed once with its triple
/// // unchanged; `db` is closed once.
/// unsafe {
///     assert_eq!(kevy_set(db, b"k".as_ptr(), 1, b"v".as_ptr(), 1, 0), 0);
///     assert_eq!(kevy_get(db, b"k".as_ptr(), 1, &mut out), 1);
///     kevy_buf_free(out.ptr, out.len, out.cap);
///     kevy_buf_free(std::ptr::null_mut(), 0, 0); // a miss's empty buffer: no-op
///     kevy_close(db);
/// }
/// ```
///
/// # Safety
/// The triple must be exactly as returned, freed exactly once.
// NO-UNWIND: drops a byte buffer, which cannot panic
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_buf_free(ptr: *mut u8, len: usize, cap: usize) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: the `# Safety` contract above covers this pointer/length pair.
    drop(unsafe { Vec::from_raw_parts(ptr, len, cap) });
}

/// Decode the packed argv the byte-array-oriented bindings send (JNI and
/// N-API both speak it): each argument is a u32-LE length prefix followed
/// by that many bytes, back to back. `None` on a truncated prefix/body or
/// zero arguments — misuse, not a protocol error.
///
/// This is a Rust-side helper for the binding shells, not part of the C ABI.
///
/// ```
/// let packed = [&3u32.to_le_bytes()[..], b"GET", &1u32.to_le_bytes(), b"k"].concat();
/// assert_eq!(kevy_ffi::unpack_argv(&packed), Some(vec![b"GET".to_vec(), b"k".to_vec()]));
/// assert_eq!(kevy_ffi::unpack_argv(&packed[..5]), None); // body cut short
/// assert_eq!(kevy_ffi::unpack_argv(&[]), None); // no command at all
/// ```
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

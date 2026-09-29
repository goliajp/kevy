//! The scalar lane: `GET` / `SET` without argv assembly or RESP encoding,
//! plus the zero-copy shared `GET` and its free. Split out of `lib.rs` for
//! the house 500-LOC rule; additive, `KEVY_ABI` unchanged.

use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::{KevyBuf, KevyDb};

/// Scalar fast path: `GET` without argv assembly or RESP encoding — the
/// raw value bytes land in `out`. Returns 1 on hit, 0 on miss, negative on
/// misuse. This is the lane the mobile bindings' hot path lives on, where
/// the bar is an mmap KV's synchronous read.
///
/// ```
/// use kevy_ffi::{KevyBuf, kevy_buf_free, kevy_close, kevy_get, kevy_open_mem, kevy_set};
///
/// let db = kevy_open_mem();
/// let mut out = KevyBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
/// // SAFETY: `db` is live; every key/value pointer covers its length; the
/// // hit's buffer is read before its single free; `db` is closed once.
/// unsafe {
///     assert_eq!(kevy_get(db, b"k".as_ptr(), 1, &mut out), 0); // miss: nothing to free
///     kevy_set(db, b"k".as_ptr(), 1, b"v1".as_ptr(), 2, 0);
///     assert_eq!(kevy_get(db, b"k".as_ptr(), 1, &mut out), 1);
///     assert_eq!(std::slice::from_raw_parts(out.ptr, out.len), b"v1");
///     kevy_buf_free(out.ptr, out.len, out.cap);
///     kevy_close(db);
/// }
/// ```
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
    // SAFETY: covered by this fn's `# Safety` contract, with the null case ruled out by
    // the checks above.
    unsafe { out.write(KevyBuf::empty()) };
    if db.is_null() || key.is_null() {
        return -1;
    }
    // SAFETY: checked non-null above; the contract requires a live `kevy_open*` handle.
    let store = unsafe { &(*db).store };
    // SAFETY: the `# Safety` contract above covers this pointer/length pair.
    let k = unsafe { std::slice::from_raw_parts(key, key_len) };
    match catch_unwind(AssertUnwindSafe(|| store.get(k))) {
        Ok(Ok(Some(v))) => {
            // SAFETY: covered by this fn's `# Safety` contract, with the null case ruled out by
            // the checks above.
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
/// [`kevy_buf_free`](crate::kevy_buf_free). 1 = hit, 0 = miss, negative = misuse.
///
/// ```
/// use kevy_ffi::{KevyBuf, kevy_buf_free_shared, kevy_close, kevy_get_shared, kevy_open_mem, kevy_set};
///
/// let db = kevy_open_mem();
/// let big = vec![b'x'; 4096]; // large enough to be shared rather than copied
/// let mut out = KevyBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
/// // SAFETY: `db` is live; every key/value pointer covers its length; the
/// // view is read before its single shared free; `db` is closed once.
/// unsafe {
///     kevy_set(db, b"blob".as_ptr(), 4, big.as_ptr(), big.len(), 0);
///     assert_eq!(kevy_get_shared(db, b"blob".as_ptr(), 4, &mut out), 1);
///     assert_eq!(std::slice::from_raw_parts(out.ptr, out.len), &big[..]);
///     kevy_buf_free_shared(out.ptr, out.len, out.cap);
///     kevy_close(db);
/// }
/// ```
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
    // SAFETY: covered by this fn's `# Safety` contract, with the null case ruled out by
    // the checks above.
    unsafe { out.write(KevyBuf::empty()) };
    if db.is_null() || key.is_null() {
        return -1;
    }
    // SAFETY: checked non-null above; the contract requires a live `kevy_open*` handle.
    let store = unsafe { &(*db).store };
    // SAFETY: the `# Safety` contract above covers this pointer/length pair.
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
            // SAFETY: covered by this fn's `# Safety` contract, with the null case ruled out by
            // the checks above.
            unsafe { out.write(KevyBuf { ptr: data, len, cap }) };
            1
        }
        Ok(Ok(None)) => 0,
        _ => -2,
    }
}

/// Free a buffer returned by [`kevy_get_shared`] — drops the engine `Arc`.
/// `ptr`/`len` are ignored; `cap` is the opaque owner handle from the shared
/// GET. Pairs 1:1 with [`kevy_get_shared`]; do NOT mix with [`kevy_buf_free`](crate::kevy_buf_free).
///
/// ```
/// use kevy_ffi::{KevyBuf, kevy_buf_free_shared, kevy_close, kevy_get_shared, kevy_open_mem, kevy_set};
///
/// let db = kevy_open_mem();
/// let mut out = KevyBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
/// // SAFETY: `db` is live; each shared buffer is freed once with its own
/// // triple; the empty sentinel of a miss is a no-op; `db` is closed once.
/// unsafe {
///     for (key, len) in [(&b"small"[..], 3), (&b"large"[..], 4096)] {
///         let val = vec![b'v'; len];
///         kevy_set(db, key.as_ptr(), key.len(), val.as_ptr(), val.len(), 0);
///         assert_eq!(kevy_get_shared(db, key.as_ptr(), key.len(), &mut out), 1);
///         assert_eq!(out.len, len);
///         kevy_buf_free_shared(out.ptr, out.len, out.cap);
///     }
///     assert_eq!(kevy_get_shared(db, b"none".as_ptr(), 4, &mut out), 0);
///     kevy_buf_free_shared(out.ptr, out.len, out.cap);
///     kevy_close(db);
/// }
/// ```
///
/// # Safety
/// `cap` must be a value produced by [`kevy_get_shared`], freed exactly once.
// NO-UNWIND: drops a byte buffer or an Arc of one, which cannot panic
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_buf_free_shared(ptr: *mut u8, len: usize, cap: usize) {
    if cap == 0 {
        return; // empty sentinel
    }
    if cap & 1 == 1 {
        // Vec-backed small value: capacity in the high bits.
        // SAFETY: the `# Safety` contract above covers this pointer/length pair.
        drop(unsafe { Vec::from_raw_parts(ptr, len, cap >> 1) });
    } else {
        // Arc-backed bulk value: cap is the Arc raw pointer.
        // SAFETY: covered by this fn's `# Safety` contract, with the null case ruled out by
        // the checks above.
        drop(unsafe { std::sync::Arc::from_raw(cap as *const Box<[u8]>) });
    }
}

/// Scalar fast path: `SET`, optionally with a TTL (`ttl_ms` 0 = none).
/// Returns 0 on success, negative on misuse or a storage error.
///
/// ```
/// use kevy_ffi::{KevyBuf, kevy_buf_free, kevy_close, kevy_cmd, kevy_open_mem, kevy_set};
///
/// let db = kevy_open_mem();
/// let argv: [&[u8]; 2] = [b"PTTL", b"session"];
/// let ptrs: Vec<*const u8> = argv.iter().map(|a| a.as_ptr()).collect();
/// let lens: Vec<usize> = argv.iter().map(|a| a.len()).collect();
/// let mut out = KevyBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
/// // SAFETY: `db` is live; every pointer covers its length; the reply is
/// // read before its single free; `db` is closed once.
/// unsafe {
///     assert_eq!(kevy_set(db, b"session".as_ptr(), 7, b"t0k3n".as_ptr(), 5, 60_000), 0);
///     assert_eq!(kevy_cmd(db, 2, ptrs.as_ptr(), lens.as_ptr(), &mut out), 0);
///     let reply = std::str::from_utf8(std::slice::from_raw_parts(out.ptr, out.len))?;
///     let ms: i64 = reply.trim_start_matches(':').trim_end().parse()?;
///     assert!(ms > 0 && ms <= 60_000); // the key now expires within a minute
///     kevy_buf_free(out.ptr, out.len, out.cap);
///     kevy_close(db);
/// }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
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
    // SAFETY: checked non-null above; the contract requires a live `kevy_open*` handle.
    let store = unsafe { &(*db).store };
    // SAFETY: the `# Safety` contract above covers this pointer/length pair.
    let k = unsafe { std::slice::from_raw_parts(key, key_len) };
    // SAFETY: the `# Safety` contract above covers this pointer/length pair.
    let v = unsafe { std::slice::from_raw_parts(val, val_len) };
    let done = catch_unwind(AssertUnwindSafe(|| {
        if ttl_ms == 0 {
            store.set(k, v).map(|_| ())
        } else {
            store.set_with_ttl(k, v, std::time::Duration::from_millis(ttl_ms)).map(|_| ())
        }
    }));
    match done {
        Ok(Ok(())) => 0,
        _ => -2,
    }
}

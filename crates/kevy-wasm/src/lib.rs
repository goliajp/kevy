//! kevy-wasm — kevy's embedded KV engine behind a hand-written C ABI.
//!
//! Compiled to `wasm32-unknown-unknown`, this crate exports a flat
//! `extern "C"` surface (no binding generator, zero dependencies beyond
//! the kevy workspace) that a small hand-written ES-module loader
//! (`pkg/kevy.js`) wraps into an idiomatic JavaScript API. The same
//! functions are plain Rust functions on native targets, which is how
//! the unit tests drive them.
//!
//! # ABI conventions
//!
//! - **Instances** are `u32` handles from [`kevy_open`]; every other
//!   call takes the handle first. `0` is never a valid handle.
//! - **Bytes in** cross as `(ptr, len)` pairs pointing into linear
//!   memory the caller obtained from [`kevy_alloc`] (and returns with
//!   [`kevy_free`]).
//! - **Bytes out** land in a per-instance result buffer read via
//!   [`kevy_out_ptr`] / [`kevy_out_len`]; the buffer is valid until the
//!   next call on the same handle, so callers copy out immediately.
//! - **Status codes**: `>= 0` is success (meaning is per-function),
//!   `-1` is an operation error (UTF-8 message in the result buffer),
//!   `-2` is an invalid handle.
//! - **Numbers** cross as `f64` where the JS side works with plain
//!   `Number` values (clocks, TTLs, counts). All are well inside the
//!   2^53 exact-integer range.
//!
//! # Threading and clocks
//!
//! The browser target has no threads and no OS clock: instances open
//! with the manual TTL reaper, the host calls [`kevy_tick`] on its own
//! cadence, and feeds `Date.now()` through [`kevy_set_clock`] first.
//!
//! # Persistence
//!
//! The browser has no filesystem, so durability is host-mediated: with
//! frame capture enabled, every write also encodes the same RESP frame
//! a kevy AOF stores on disk. The host pumps [`kevy_aof_frames_out`]
//! into its own storage (OPFS, IndexedDB, anything append-capable) and
//! feeds the log back through [`kevy_aof_frame_in`] on the next open.
//! [`kevy_aof_dump`] produces a compacted image for log rewriting. The
//! byte format is exactly `kevy-persist`'s AOF format, so a log written
//! by a browser tab replays in a native kevy just as well.

pub mod abi_aof;
pub mod abi_cmd;
pub mod abi_core;
pub mod abi_kv;
pub mod abi_pubsub;

#[cfg(test)]
#[path = "abi_tests.rs"]
mod tests;

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use kevy_embedded::{Store, Subscription};
use kevy_store::{KevyError, StoreError};

/// ABI contract version reported by [`abi_core::kevy_abi_version`].
/// Bumped on any incompatible change to the export surface or the
/// packed byte formats, so loaders can refuse a mismatched module.
pub const ABI_VERSION: u32 = 1;

/// Success status.
pub(crate) const OK: i32 = 0;
/// Operation failed; the result buffer holds a UTF-8 error message.
pub(crate) const ERR: i32 = -1;
/// The handle does not name a live instance.
pub(crate) const BAD_HANDLE: i32 = -2;

/// One open store plus its ABI-side state.
pub(crate) struct Instance {
    pub(crate) store: Store,
    /// Live subscriptions by subscription id (ids are per-instance).
    pub(crate) subs: BTreeMap<u32, Subscription>,
    pub(crate) next_sub: u32,
    /// Whether writes also encode AOF frames into `aof_out`.
    pub(crate) capture_aof: bool,
    /// Pending AOF frames awaiting a `kevy_aof_frames_out` drain.
    pub(crate) aof_out: Vec<u8>,
    /// Unparsed tail carried between `kevy_aof_frame_in` chunks.
    pub(crate) aof_in_carry: Vec<u8>,
    /// Whether the inbound AOF stream is past its optional magic header.
    pub(crate) aof_in_started: bool,
    /// Format of the host's stored log — set from the magic when the
    /// host feeds its log back (v1 read-forever contract), flipped to
    /// V2 by `kevy_aof_dump` (the image replaces the log). Outbound
    /// frames encode in THIS format so the host's verbatim appends
    /// never mix formats within one log. Fresh logs are V2.
    pub(crate) aof_format: kevy_persist::AofFormat,
    /// Whether the host's log already carries its format marker — true
    /// once the host fed any log bytes or a dump image replaced the
    /// log. While false, the first captured V2 frame is preceded by
    /// the `KEVYAOF2` magic so a fresh log is self-describing.
    pub(crate) aof_out_started: bool,
    /// Reusable payload buffer for v2 record encoding.
    pub(crate) aof_scratch: Vec<u8>,
    /// Result buffer exposed through `kevy_out_ptr` / `kevy_out_len`.
    pub(crate) out: Vec<u8>,
}

impl Instance {
    pub(crate) fn new(store: Store, capture_aof: bool) -> Self {
        Instance {
            store,
            subs: BTreeMap::new(),
            next_sub: 1,
            capture_aof,
            aof_out: Vec::new(),
            aof_in_carry: Vec::new(),
            aof_in_started: false,
            aof_format: kevy_persist::AofFormat::V2,
            aof_out_started: false,
            aof_scratch: Vec::new(),
            out: Vec::new(),
        }
    }

    /// Set the result buffer to `bytes`.
    pub(crate) fn put_out(&mut self, bytes: &[u8]) {
        self.out.clear();
        self.out.extend_from_slice(bytes);
    }

    /// Record an error message in the result buffer and return [`ERR`].
    pub(crate) fn fail(&mut self, msg: impl std::fmt::Display) -> i32 {
        self.out.clear();
        // Writing into a Vec cannot fail.
        let _ = write!(self.out, "{msg}");
        ERR
    }

    /// Record a [`KevyError`] using the Redis-canonical wording for
    /// store-semantic errors, then return [`ERR`].
    ///
    /// The engine keeps errors structured (`KevyError::Store(WrongType)`),
    /// and its `Display` prints the internal Debug spelling
    /// (`store error: WrongType`) — an implementation detail that must not
    /// leak to a JS caller. At the door boundary we translate store
    /// variants to the exact messages a real Redis emits (`WRONGTYPE …`),
    /// so the JS-visible error reads like a genuine server error. Non-store
    /// variants keep their `Display` text.
    pub(crate) fn fail_kevy(&mut self, e: &KevyError) -> i32 {
        match e {
            KevyError::Store(se) => self.fail(store_err_canonical(se)),
            other => self.fail(other),
        }
    }

    /// Append one command as an AOF frame to the pending pump buffer
    /// (no-op unless frame capture was requested at open). Encodes in
    /// the host log's format — a bare RESP frame for a v1-era log, a
    /// checksummed v2 record otherwise; a fresh v2 log gets its
    /// `KEVYAOF2` magic ahead of the first frame so the stored bytes
    /// are self-describing on the next open.
    pub(crate) fn log_frame(&mut self, parts: &[&[u8]]) {
        if !self.capture_aof {
            return;
        }
        let argv = kevy_persist::Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>());
        // Vec is an infallible Write.
        match self.aof_format {
            kevy_persist::AofFormat::V1 => {
                let _ = kevy_persist::write_multibulk(&mut self.aof_out, &argv);
            }
            kevy_persist::AofFormat::V2 => {
                if !self.aof_out_started {
                    self.aof_out.extend_from_slice(kevy_persist::AOF2_MAGIC);
                    self.aof_out_started = true;
                }
                let _ = kevy_persist::write_record_multibulk(
                    &mut self.aof_out,
                    &argv,
                    &mut self.aof_scratch,
                );
            }
        }
    }
}

/// Redis-canonical message for a store-semantic error.
///
/// Mirrors the strings kevy-embedded's full RESP dispatcher emits
/// (`dispatch::util`), duplicated here because that table is `pub(super)`
/// and unreachable from this crate. The `dispatch_oracle` parity test in
/// kevy-embedded holds those strings against the server byte for byte, so
/// this door surfaces exactly the wording a native kevy would.
fn store_err_canonical(e: &StoreError) -> &'static str {
    match e {
        StoreError::WrongType => {
            "WRONGTYPE Operation against a key holding the wrong kind of value"
        }
        StoreError::NotInteger => "ERR value is not an integer or out of range",
        StoreError::Overflow => "ERR increment or decrement would overflow",
        StoreError::OutOfRange => "ERR index out of range",
        StoreError::NoSuchKey => "ERR no such key",
        StoreError::NotFloat => "ERR value is not a valid float",
        StoreError::OutOfMemory => "OOM command not allowed when used memory > 'maxmemory'.",
    }
}

/// Handle allocator. Starts at 1 so 0 stays "no instance".
pub(crate) static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// The live instance table. A `Mutex` (never contended on the
/// single-threaded wasm target) keeps the native test builds sound.
pub(crate) static REG: Mutex<BTreeMap<u32, Instance>> = Mutex::new(BTreeMap::new());

/// Allocate a fresh handle id.
pub(crate) fn next_id() -> u32 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Run `f` against the instance behind `h`, or return `missing` when the
/// handle is not live.
pub(crate) fn with<R>(h: u32, missing: R, f: impl FnOnce(&mut Instance) -> R) -> R {
    let mut reg = REG.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match reg.get_mut(&h) {
        Some(inst) => f(inst),
        None => missing,
    }
}

/// View a caller-provided `(ptr, len)` pair as a byte slice for the
/// duration of the current call.
///
/// # Safety
///
/// `ptr` must point to `len` readable bytes that stay valid (and are not
/// written) for the whole ABI call — the loader guarantees this by only
/// passing buffers it obtained from [`abi_core::kevy_alloc`] and not
/// touching them until the call returns. `len == 0` is always safe and
/// yields the empty slice.
pub(crate) unsafe fn arg<'a>(ptr: *const u8, len: u32) -> &'a [u8] {
    if len == 0 {
        return &[];
    }
    // SAFETY: contract above — caller passes a live, non-aliased buffer.
    unsafe { std::slice::from_raw_parts(ptr, len as usize) }
}

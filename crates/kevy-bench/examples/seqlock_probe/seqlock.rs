//! Seqlock-protected shared-read cell over the real `kevy_store::Value` +
//! an EBR-lite deferred-reclamation scheme.
//!
//! v4 T9b / K-903 L1 pre-work gate PROTOTYPE — not production code. Scope
//! deliberately mirrors the shared-read keyspace design under judgement:
//!
//! - **Writes stay shard-owned**: exactly ONE writer thread per entry set
//!   (the owner shard). The write path is `&self` (interior atomics) but is
//!   only ever called from the owning thread — the shared-nothing write
//!   lane is unchanged.
//! - **Reads are shared**: any thread may call [`SeqEntry::read`] on any
//!   entry while holding an [`Ebr`] pin.
//! - **Structural map ops (insert/rehash) are out of scope**: the map is
//!   pre-populated and its bucket array never moves during the concurrent
//!   phase. The full design needs a table-level version + epoch-deferred
//!   bucket-array swap for those; this prototype answers the per-entry
//!   value-read question only (see the gate report).
//!
//! Memory-model shape: the classic two-counter seqlock (Boehm, "Can
//! seqlocks get along with programming language memory models?") with the
//! value bit-image held in `AtomicU64` words so the racing snapshot copy is
//! *defined behaviour* (no `UnsafeCell` byte race). Heap pointers inside a
//! validated snapshot (`SmallBytes` spilled ≥ 23 B, `Arc<Box<[u8]>>` bulk)
//! are only dereferenced while pinned; the writer retires displaced values
//! through the epoch queue instead of dropping them inline.

use kevy_store::Value;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering, fence};

/// `Value` is asserted 32 B in kevy-store; four u64 words carry its bit-image.
pub const VAL_WORDS: usize = 4;
const _: () = assert!(std::mem::size_of::<Value>() == 8 * VAL_WORDS);

/// Sequence word + expire word + 4 value words = 48 B. Aligned to 128 B
/// (Apple M-series line pair / lx64 spatial prefetcher pair) so disjoint
/// entries never share a line — this IS the "per-entry version word"
/// design under test (vs one shared table version; see perfsim cell F).
#[repr(C, align(128))]
pub struct SeqEntry {
    seq: AtomicU64,
    expire_at_ns: AtomicU64, // 0 = no TTL
    words: [AtomicU64; VAL_WORDS],
}

/// Encode a `Value` into a frozen word image. Writing through a
/// zero-initialised buffer freezes the enum's padding bytes so reading
/// them back as `u64` is defined.
fn encode(v: Value) -> [u64; VAL_WORDS] {
    let mut img = [0u64; VAL_WORDS];
    // SAFETY: img is 32 B and u64-aligned (Value's align is 8). `v` is
    // moved into the buffer; ownership now lives in the bit-image.
    unsafe { std::ptr::write(img.as_mut_ptr().cast::<Value>(), v) };
    img
}

/// Outcome of a shared read.
pub enum ReadHit {
    /// Value bytes were appended to `out` (Str inline/heap, or ArcBulk in
    /// copy mode).
    Bytes,
    /// `Value::Int` payload (caller formats; nothing appended).
    Int(i64),
    /// ArcBulk in zero-copy mode: the refcount was bumped while pinned, so
    /// the Arc stays valid after unpin (the writev-iovec reply shape).
    Arc(Arc<Box<[u8]>>),
    /// Live entry exists but its TTL has passed → reads see a miss. No
    /// mutation (owner's reaper reclaims), same as `Store::get_shared`.
    Expired,
}

impl SeqEntry {
    pub fn new(v: Value, expire_at_ns: u64) -> Self {
        SeqEntry {
            seq: AtomicU64::new(0),
            expire_at_ns: AtomicU64::new(expire_at_ns),
            words: encode(v).map(AtomicU64::new),
        }
    }

    /// Owner-shard overwrite (the SET path). Single writer per entry.
    /// Returns the displaced `Value` — the caller MUST retire it through
    /// the [`RetireQueue`] (not drop it inline) so a concurrent reader
    /// holding a validated snapshot can still dereference its pointers.
    pub fn write(&self, v: Value, expire_at_ns: u64) -> Value {
        let img = encode(v);
        let mut old = [0u64; VAL_WORDS];
        for (o, w) in old.iter_mut().zip(&self.words) {
            *o = w.load(Ordering::Relaxed); // single writer: stable
        }
        let s = self.seq.load(Ordering::Relaxed);
        // Open the write section (odd). The Release fence orders this
        // store before the data stores from any validating reader's view.
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        for (w, i) in self.words.iter().zip(img) {
            w.store(i, Ordering::Relaxed);
        }
        self.expire_at_ns.store(expire_at_ns, Ordering::Relaxed);
        // Close (even): Release publishes the data to readers' Acquire v1.
        self.seq.store(s.wrapping_add(2), Ordering::Release);
        // SAFETY: `old` is the complete bit-image of the Value this cell
        // owned (we are the single writer). Ownership moves to the caller.
        unsafe { std::ptr::read(old.as_ptr().cast::<Value>()) }
    }

    /// Shared read (the GET path). Caller must hold an [`Ebr`] pin for the
    /// whole call (and, in `arc_mode`, only needs it for the call itself —
    /// the returned Arc owns a refcount).
    ///
    /// Returns the outcome and the number of retries (odd-seq observations
    /// + failed validations) this read burned.
    #[inline]
    pub fn read(&self, now_ns: u64, out: &mut Vec<u8>, arc_mode: bool) -> (ReadHit, u32) {
        let mut retries = 0u32;
        loop {
            let v1 = self.seq.load(Ordering::Acquire);
            if v1 & 1 == 0 {
                let mut img = [0u64; VAL_WORDS];
                for (o, w) in img.iter_mut().zip(&self.words) {
                    *o = w.load(Ordering::Relaxed);
                }
                let exp = self.expire_at_ns.load(Ordering::Relaxed);
                // Acquire fence: if any data load above observed a write
                // made after the writer's Release fence, the seq load
                // below observes the odd seq → validation fails.
                fence(Ordering::Acquire);
                if self.seq.load(Ordering::Relaxed) == v1 {
                    if exp != 0 && now_ns >= exp {
                        return (ReadHit::Expired, retries);
                    }
                    // SAFETY: validated bit-image of a Value this cell
                    // held; heap pointers inside it stay live while we are
                    // pinned (writer retires via EBR, never frees inline).
                    let val = unsafe { &*img.as_ptr().cast::<ManuallyDrop<Value>>() };
                    match &**val {
                        Value::Str(s) => {
                            out.extend_from_slice(s.as_slice());
                            return (ReadHit::Bytes, retries);
                        }
                        Value::Int(n) => return (ReadHit::Int(*n), retries),
                        Value::ArcBulk(a) => {
                            if arc_mode {
                                // Refcount bump while pinned: the ArcInner
                                // is guaranteed live (EBR), so the clone is
                                // sound and outlives the pin.
                                return (ReadHit::Arc(Arc::clone(a)), retries);
                            }
                            let bytes: &[u8] = a;
                            out.extend_from_slice(bytes);
                            return (ReadHit::Bytes, retries);
                        }
                        // The prototype's write set is Str/Int/ArcBulk
                        // only. By-argument unreachable → fall back to the
                        // type name (house rule: no bare unreachable!).
                        other => {
                            out.extend_from_slice(other.type_name().as_bytes());
                            return (ReadHit::Bytes, retries);
                        }
                    }
                }
            }
            retries += 1;
            std::hint::spin_loop();
        }
    }
}

// ---------------------------------------------------------------------------
// EBR-lite: per-reader epoch slots + writer-local retire queues.
// ---------------------------------------------------------------------------

#[repr(align(128))]
struct Slot(AtomicU64);

/// Epoch value published by a quiescent (unpinned) reader slot.
pub const QUIESCENT: u64 = u64::MAX;

/// Minimal epoch-based-reclamation core: one global epoch, one padded slot
/// per reader. Writers are per-shard single-owner, so retire queues live
/// writer-local ([`RetireQueue`]) with no cross-writer synchronisation.
pub struct Ebr {
    global: AtomicU64,
    slots: Box<[Slot]>,
}

impl Ebr {
    pub fn new(readers: usize) -> Self {
        Ebr {
            global: AtomicU64::new(1),
            slots: (0..readers).map(|_| Slot(AtomicU64::new(QUIESCENT))).collect(),
        }
    }

    /// Pin reader `id`. Crossbeam-style store-then-recheck loop closes the
    /// race where the writer advances + scans between our global load and
    /// our slot store.
    #[inline]
    pub fn pin(&self, id: usize) {
        loop {
            let e = self.global.load(Ordering::SeqCst);
            self.slots[id].0.store(e, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            if self.global.load(Ordering::SeqCst) == e {
                return;
            }
        }
    }

    #[inline]
    pub fn unpin(&self, id: usize) {
        self.slots[id].0.store(QUIESCENT, Ordering::Release);
    }

    pub fn epoch(&self) -> u64 {
        self.global.load(Ordering::SeqCst)
    }

    pub fn advance(&self) {
        self.global.fetch_add(1, Ordering::SeqCst);
    }

    /// Smallest epoch any pinned reader holds ([`QUIESCENT`] when none).
    pub fn min_active(&self) -> u64 {
        fence(Ordering::SeqCst);
        self.slots
            .iter()
            .map(|s| s.0.load(Ordering::SeqCst))
            .min()
            .unwrap_or(QUIESCENT)
    }
}

/// Writer-local deferred-drop queue. Values displaced by an overwrite park
/// here until every reader pinned at-or-before their retirement epoch has
/// unpinned, then drop for real.
pub struct RetireQueue {
    items: std::collections::VecDeque<(u64, Value)>,
    /// Collect (advance + sweep) once this many items are parked.
    threshold: usize,
    pub retired: u64,
    pub freed: u64,
    /// High-water mark of parked items (bounded-memory evidence).
    pub max_parked: usize,
}

impl RetireQueue {
    pub fn new(threshold: usize) -> Self {
        RetireQueue {
            items: std::collections::VecDeque::new(),
            threshold,
            retired: 0,
            freed: 0,
            max_parked: 0,
        }
    }

    /// Park a displaced value tagged with the current epoch.
    pub fn retire(&mut self, ebr: &Ebr, v: Value) {
        self.items.push_back((ebr.epoch(), v));
        self.retired += 1;
        self.max_parked = self.max_parked.max(self.items.len());
        if self.items.len() >= self.threshold {
            ebr.advance();
            self.collect(ebr);
        }
    }

    /// Drop every parked value whose retirement epoch is at least two
    /// epochs behind the oldest active reader (one epoch of slack on top
    /// of the pin loop's guarantee — cheap belt over braces).
    pub fn collect(&mut self, ebr: &Ebr) {
        let min = ebr.min_active();
        while let Some((e, _)) = self.items.front() {
            let safe = min == QUIESCENT || e.wrapping_add(2) <= min;
            if !safe {
                break;
            }
            drop(self.items.pop_front());
            self.freed += 1;
        }
    }

    /// End-of-run drain — only sound once every reader thread has joined.
    pub fn drain_all(&mut self) {
        self.freed += self.items.len() as u64;
        self.items.clear();
    }
}

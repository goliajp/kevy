//! Batched home-shipping of foreign frees.
//!
//! M1 convicted the per-op foreign path: same-shard KV measured ±1 %
//! while cross-shard KV paid 18–39 %, and the per-free bill was three
//! atomic RMWs on the owning segment's header line — a CAS-loop push
//! plus two `fetch_add`s — with up to seven shards hammering the same
//! line (the foreign-frees KV finding measured the per-op bill).
//!
//! glibc dodges that bill by letting the freeing thread keep and reuse
//! the foreign chunk locally. We deliberately do not: once a slot can be
//! re-handed-out by a non-owner, a *third* shard freeing it later cannot
//! recover who currently accounts for it — the address only reveals the
//! segment's owner — and keeping the identity exact would put an atomic
//! back on every foreign alloc *and* free. That is not a ceiling; it is
//! the same bill at a different counter.
//!
//! The ceiling that keeps ownership exact is **amortisation**: the free
//! fast path appends to this heap-local ring — two plain stores, zero
//! cross-core traffic — and the flush ships a whole batch home as one
//! pre-linked chain per segment: one CAS splice and two `fetch_add`s
//! for the lot. The owner's drain sees exactly the format it always saw.
//!
//! Accounting stays exact throughout. A pending slot's bit is still set,
//! so the owner's counters still cover its bytes — the same staleness
//! window today's un-drained list has, just entered a flush later.
//! The flush's `fetch_add`s make the parked bytes visible; the owner's
//! drain settles them. `balanced()` holds at every instant.

use core::ptr::NonNull;

use crate::class;
use crate::segment::{self, Segment};

/// Foreign frees held locally before a flush. 128 entries is ~1.8 KB of
/// heap and bounds both the amortisation batch and what a thread that
/// exits without flushing can strand (its ring leaks with the heap —
/// bits stay set, bounded by this capacity).
const CAP: usize = 128;

/// Distinct owning segments a single flush can chain concurrently. In
/// practice one hot owner dominates; overflowing this just flushes a
/// group early, it never drops anything.
const GROUPS: usize = 8;

/// One pending foreign free: the slot, what the caller asked for, and
/// its class (needed for slot-size sums at flush; not recoverable from
/// the request size alone).
#[derive(Clone, Copy)]
struct Pending {
    addr: usize,
    requested: u32,
    class: u8,
}

/// The heap-local ring of foreign frees awaiting shipment.
pub(crate) struct Outbound {
    entries: [Pending; CAP],
    len: u16,
}

impl Outbound {
    pub(crate) const fn new() -> Self {
        Self { entries: [Pending { addr: 0, requested: 0, class: 0 }; CAP], len: 0 }
    }

    /// Record a foreign free. Returns `false` when the ring is full —
    /// the caller flushes and retries.
    pub(crate) fn push(&mut self, ptr: NonNull<u8>, requested: usize, class: usize) -> bool {
        if self.len as usize == CAP {
            return false;
        }
        self.entries[self.len as usize] = Pending {
            addr: ptr.as_ptr() as usize,
            requested: requested as u32,
            class: class as u8,
        };
        self.len += 1;
        true
    }

    /// Whether anything is waiting.
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Ship everything home: group by owning segment, link each group
    /// into a chain through the slots themselves (their lines are still
    /// warm — this heap's caller freed them moments ago), and splice
    /// each chain onto its segment's list with one CAS.
    pub(crate) fn flush(&mut self) {
        let mut groups: [Group; GROUPS] = [Group::EMPTY; GROUPS];
        for i in 0..self.len as usize {
            let e = self.entries[i];
            // SAFETY: every entry was a live slot address when pushed,
            // and pending slots are exclusively ours until spliced.
            let seg = unsafe {
                segment::segment_of(NonNull::new_unchecked(e.addr as *mut u8))
            };
            let slot = groups
                .iter_mut()
                .find(|g| g.seg == seg.as_ptr() || g.seg.is_null());
            let g = match slot {
                Some(g) => g,
                // All group slots busy with other segments: ship the
                // fullest one now and reuse its slot. Nothing is lost,
                // one group just amortises less this once.
                None => {
                    let g = groups.iter_mut().max_by_key(|g| g.count).unwrap();
                    g.ship();
                    g
                }
            };
            if g.seg.is_null() {
                g.seg = seg.as_ptr();
            }
            g.link(e);
        }
        for g in &mut groups {
            g.ship();
        }
        self.len = 0;
    }
}

/// One per-segment chain being assembled during a flush.
#[derive(Clone, Copy)]
struct Group {
    seg: *mut Segment,
    head: *mut u8,
    tail: *mut u8,
    live_sum: usize,
    bytes_sum: usize,
    count: u32,
}

impl Group {
    const EMPTY: Self = Self {
        seg: core::ptr::null_mut(),
        head: core::ptr::null_mut(),
        tail: core::ptr::null_mut(),
        live_sum: 0,
        bytes_sum: 0,
        count: 0,
    };

    /// Thread one pending slot onto this group's chain, in the exact
    /// format the owner's drain has always read: link word first, the
    /// requested size beside it.
    fn link(&mut self, e: Pending) {
        let p = e.addr as *mut u8;
        // SAFETY: the slot is ours until spliced; its first bytes are
        // free to carry the link and size (every class is ≥ 16 B).
        unsafe {
            p.cast::<*mut u8>().write(self.head);
            p.add(segment::FOREIGN_SIZE_OFFSET).cast::<u32>().write(e.requested);
        }
        if self.head.is_null() {
            self.tail = p;
        }
        self.head = p;
        self.live_sum += e.requested as usize;
        self.bytes_sum += class::size_of(e.class as usize);
        self.count += 1;
    }

    /// Splice the assembled chain onto the segment's foreign list and
    /// post the batch's byte sums — the whole batch's cross-core bill.
    fn ship(&mut self) {
        if self.seg.is_null() || self.head.is_null() {
            *self = Self::EMPTY;
            return;
        }
        // SAFETY: group segments are live headers (segments are never
        // unmapped while a heap that freed into them is running — even
        // an exited owner leaks its mapping rather than unmapping it).
        let seg = unsafe { &*self.seg };
        // SAFETY: head..tail is a chain of slots exclusively ours until
        // the CAS below publishes it.
        unsafe {
            segment::splice_foreign(seg, self.head, self.tail, self.live_sum, self.bytes_sum);
        }
        *self = Self::EMPTY;
    }
}

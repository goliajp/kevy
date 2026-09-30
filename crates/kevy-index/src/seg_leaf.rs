//! One leaf of a segment's tree: a single fixed-size allocation holding
//! its entries packed.
//!
//! Slots grow up from the front, one per entry in order: the first 8
//! bytes of the entry's order key (zero-padded) and the offset of its
//! tail. Tails grow down from the back: a tag (the order key's length,
//! doubled, plus one when the rest lives out of line), the payload's
//! length when the tree stores payloads, then either the order key past
//! its first 8 bytes followed by the payload, or — for an entry too big
//! to share a page — the number of its [`Overflow`] slab. A search
//! compares the 8-byte heads as integers and reads a tail only when two
//! heads tie.

use std::cmp::Ordering;

use crate::seg_codec::varint;

/// Bytes of one leaf allocation. glibc hands out a 1792-byte chunk for
/// it, which fits the 1808-byte hole a demoted row leaves behind.
pub(crate) const LEAF_BYTES: usize = 1784;
const BUF: usize = LEAF_BYTES - 16;
const SLOT: usize = 10;
/// Tails longer than this live out of line, so a leaf always holds a
/// handful of entries.
const MAX_INLINE: usize = 240;

/// No leaf.
pub(crate) const NIL: u32 = u32::MAX;

/// An order key being searched for: its first 8 bytes as an integer
/// (zero-padded) and all of it. `past` makes it sort after every key it
/// is a prefix of — the upper bound of "every entry with this value".
#[derive(Debug, Clone, Copy)]
pub(crate) struct Probe<'a> {
    pub(crate) head: u64,
    /// The first 16 bytes, for the inner nodes' wider heads.
    pub(crate) head16: u128,
    pub(crate) bytes: &'a [u8],
    pub(crate) past: bool,
}

impl<'a> Probe<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Probe<'a> {
        Probe { head: head_of(bytes), head16: head16_of(bytes), bytes, past: false }
    }

    pub(crate) fn past(bytes: &'a [u8]) -> Probe<'a> {
        Probe { head: head_of(bytes), head16: head16_of(bytes), bytes, past: true }
    }
}

/// The first 8 bytes of `e` as a big-endian integer, zero-padded.
pub(crate) fn head_of(e: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    let n = e.len().min(8);
    a[..n].copy_from_slice(&e[..n]);
    u64::from_be_bytes(a)
}

/// The first 16 bytes of `e` as a big-endian integer, zero-padded.
pub(crate) fn head16_of(e: &[u8]) -> u128 {
    let mut a = [0u8; 16];
    let n = e.len().min(16);
    a[..n].copy_from_slice(&e[..n]);
    u128::from_be_bytes(a)
}

/// Byte-wise order of two slices, without a call for the short ones an
/// order key's tail usually is: zero-padded, a tie leaves the shorter a
/// prefix of the longer, which sorts first.
#[inline]
pub(crate) fn cmp_bytes(a: &[u8], b: &[u8]) -> Ordering {
    if a.len() <= 8 && b.len() <= 8 {
        return head_of(a).cmp(&head_of(b)).then(a.len().cmp(&b.len()));
    }
    a.cmp(b)
}

/// Compare a probe with a stored key given as its head, length and the
/// bytes past its head.
pub(crate) fn cmp_key(p: &Probe<'_>, head: u64, len: usize, rest: &[u8]) -> Ordering {
    if !p.past {
        return match p.head.cmp(&head) {
            Ordering::Equal if p.bytes.len().min(len) < 8 => p.bytes.len().cmp(&len),
            Ordering::Equal => cmp_bytes(&p.bytes[8..], rest),
            o => o,
        };
    }
    // past: compare the bytes both have; on a tie the probe is either a
    // prefix of the key or the key a prefix of it, and sorts after both
    let (n, hb) = (p.bytes.len(), head.to_be_bytes());
    let m = n.min(len);
    let a = m.min(8);
    match p.bytes[..a].cmp(&hb[..a]) {
        Ordering::Equal if m > 8 => match p.bytes[8..m].cmp(&rest[..m - 8]) {
            Ordering::Equal => Ordering::Greater,
            o => o,
        },
        Ordering::Equal => Ordering::Greater,
        o => o,
    }
}

fn varint_len(n: usize) -> usize {
    let mut len = 1;
    let mut n = n >> 7;
    while n > 0 {
        len += 1;
        n >>= 7;
    }
    len
}

/// Write a LEB128 varint into `buf` at `at`; the index past it.
fn write_varint(buf: &mut [u8], mut at: usize, mut n: usize) -> usize {
    while n >= 0x80 {
        buf[at] = (n as u8) | 0x80;
        n >>= 7;
        at += 1;
    }
    buf[at] = n as u8;
    at + 1
}

/// Bytes of the tail starting at `start` in `buf`, and its slab if it
/// has one.
fn tail_len_in(buf: &[u8], start: usize, shape: Shape) -> (usize, Option<u32>) {
    let mut at = start;
    let tag = varint(buf, &mut at);
    let plen = if shape.payloads { varint(buf, &mut at) } else { 0 };
    if shape.vlens {
        varint(buf, &mut at);
    }
    if tag % 2 == 1 {
        let id =
            u32::from_le_bytes(buf[at..at + 4].try_into().expect(
                "an out-of-line tail stores its slab id in the four bytes after its varints",
            ));
        return (at + 4 - start, Some(id));
    }
    (at + (tag / 2).saturating_sub(8) + plen - start, None)
}

/// Out-of-line tails of entries too big for a page, by number.
#[derive(Debug, Default)]
pub(crate) struct Overflow {
    slabs: Vec<Box<[u8]>>,
    free: Vec<u32>,
    pub(crate) bytes: usize,
}

impl Overflow {
    fn put(&mut self, b: Vec<u8>) -> u32 {
        self.bytes += b.len();
        let b = b.into_boxed_slice();
        match self.free.pop() {
            Some(id) => {
                self.slabs[id as usize] = b;
                id
            }
            None => {
                self.slabs.push(b);
                (self.slabs.len() - 1) as u32
            }
        }
    }

    fn take(&mut self, id: u32) {
        let b = std::mem::take(&mut self.slabs[id as usize]);
        self.bytes -= b.len();
        self.free.push(id);
    }

    pub(crate) fn get(&self, id: u32) -> &[u8] {
        &self.slabs[id as usize]
    }
}

/// One entry on its way in: the order key, the length of the value at its
/// start (kept only by trees whose [`Shape`] asks), and the payload.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Ent<'a> {
    pub(crate) key: &'a [u8],
    pub(crate) vlen: usize,
    pub(crate) payload: &'a [u8],
}

/// What a tree's tails carry besides the order key: a payload, and the
/// length of the value at the key's start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shape {
    pub(crate) payloads: bool,
    pub(crate) vlens: bool,
}

#[derive(Debug)]
pub(crate) struct Leaf {
    n: u16,
    top: u16,
    dead: u16,
    shape: Shape,
    pub(crate) next: u32,
    pub(crate) prev: u32,
    buf: [u8; BUF],
}

/// One entry's tail, read in place.
#[derive(Debug)]
pub(crate) struct Tail<'a> {
    pub(crate) len: usize,
    pub(crate) rest: &'a [u8],
    pub(crate) payload: &'a [u8],
    /// The value's length at the key's start, when the tree keeps it.
    pub(crate) vlen: usize,
}

impl Leaf {
    pub(crate) fn new(shape: Shape) -> Box<Leaf> {
        Box::new(Leaf {
            n: 0,
            top: BUF as u16,
            dead: 0,
            shape,
            next: NIL,
            prev: NIL,
            buf: [0; BUF],
        })
    }

    pub(crate) fn len(&self) -> usize {
        usize::from(self.n)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Page bytes the tail of an entry with an order key of `len` bytes
    /// and a payload of `plen` bytes takes, and whether it goes out of line.
    fn tail_bytes(&self, len: usize, plen: usize, vlen: usize) -> (usize, bool) {
        let meta = varint_len(2 * len + 1)
            + if self.shape.payloads { varint_len(plen) } else { 0 }
            + if self.shape.vlens { varint_len(vlen) } else { 0 };
        let body = len.saturating_sub(8) + plen;
        if body > MAX_INLINE { (meta + 4, true) } else { (meta + body, false) }
    }

    /// Live bytes: slots and tails still in use.
    pub(crate) fn used(&self) -> usize {
        self.len() * SLOT + BUF - usize::from(self.top) - usize::from(self.dead)
    }

    pub(crate) const fn capacity() -> usize {
        BUF
    }

    fn free(&self) -> usize {
        usize::from(self.top) - self.len() * SLOT
    }

    #[inline(always)]
    pub(crate) fn head(&self, i: usize) -> u64 {
        let s = i * SLOT;
        let mut a = [0u8; 8];
        a.copy_from_slice(&self.buf[s..s + 8]);
        u64::from_be_bytes(a)
    }

    fn off(&self, i: usize) -> usize {
        let s = i * SLOT + 8;
        usize::from(u16::from_le_bytes([self.buf[s], self.buf[s + 1]]))
    }

    #[inline(always)]
    pub(crate) fn tail<'a>(&'a self, i: usize, ov: &'a Overflow) -> Tail<'a> {
        let start = self.off(i);
        let mut at = start;
        let tag = varint(&self.buf, &mut at);
        let len = tag / 2;
        let plen = if self.shape.payloads { varint(&self.buf, &mut at) } else { 0 };
        let vlen = if self.shape.vlens { varint(&self.buf, &mut at) } else { 0 };
        let rest_len = len.saturating_sub(8);
        if tag % 2 == 1 {
            let id = u32::from_le_bytes(self.buf[at..at + 4].try_into().expect(
                "an out-of-line tail stores its slab id in the four bytes after its varints",
            ));
            let slab = ov.get(id);
            return Tail {
                len,
                rest: &slab[..rest_len],
                payload: &slab[rest_len..rest_len + plen],
                vlen,
            };
        }
        let rest_end = at + rest_len;
        let end = rest_end + plen;
        Tail { len, rest: &self.buf[at..rest_end], payload: &self.buf[rest_end..end], vlen }
    }

    /// Page bytes of entry `i`'s tail (no overflow access needed).
    fn tail_len(&self, i: usize) -> (usize, Option<u32>) {
        tail_len_in(&self.buf, self.off(i), self.shape)
    }

    /// Entry `i`'s order key, appended to `out`.
    pub(crate) fn key_into(&self, i: usize, ov: &Overflow, out: &mut Vec<u8>) {
        let t = self.tail(i, ov);
        let h = self.head(i).to_be_bytes();
        out.extend_from_slice(&h[..t.len.min(8)]);
        out.extend_from_slice(t.rest);
    }

    /// Whether entry `i`'s order key starts with `pre`.
    pub(crate) fn starts_with(&self, i: usize, pre: &[u8], ov: &Overflow) -> bool {
        let t = self.tail(i, ov);
        if t.len < pre.len() {
            return false;
        }
        let h = self.head(i).to_be_bytes();
        let n = pre.len().min(8);
        h[..n] == pre[..n] && (pre.len() <= 8 || t.rest.starts_with(&pre[8..]))
    }

    pub(crate) fn cmp_at(&self, p: &Probe<'_>, i: usize, ov: &Overflow) -> Ordering {
        let head = self.head(i);
        // a short `past` probe can tie with keys whose padded heads differ
        if p.head != head && (!p.past || p.bytes.len() >= 8) {
            return p.head.cmp(&head);
        }
        let t = self.tail(i, ov);
        cmp_key(p, head, t.len, t.rest)
    }

    /// The first slot whose key is not below the probe.
    pub(crate) fn lower_bound(&self, p: &Probe<'_>, ov: &Overflow) -> usize {
        let (mut lo, mut hi) = (0, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.cmp_at(p, mid, ov) == Ordering::Greater {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Put `(key, payload)` in at slot `i`; `false` when it does not fit.
    pub(crate) fn insert_at(&mut self, i: usize, e: Ent<'_>, ov: &mut Overflow) -> bool {
        let Ent { key, vlen, payload } = e;
        let (tail, out_of_line) = self.tail_bytes(key.len(), payload.len(), vlen);
        if !self.make_room(SLOT + tail) {
            return false;
        }
        let top = usize::from(self.top) - tail;
        let mut at = write_varint(&mut self.buf, top, 2 * key.len() + usize::from(out_of_line));
        if self.shape.payloads {
            at = write_varint(&mut self.buf, at, payload.len());
        }
        if self.shape.vlens {
            at = write_varint(&mut self.buf, at, vlen);
        }
        let rest = key.get(8..).unwrap_or(&[]);
        if out_of_line {
            let id = ov.put([rest, payload].concat());
            self.buf[at..at + 4].copy_from_slice(&id.to_le_bytes());
        } else {
            self.buf[at..at + rest.len()].copy_from_slice(rest);
            at += rest.len();
            self.buf[at..at + payload.len()].copy_from_slice(payload);
        }
        self.top = top as u16;
        self.put_slot(i, head_of(key), top);
        true
    }

    /// Whether `need` bytes fit, compacting the tails if that makes them.
    fn make_room(&mut self, need: usize) -> bool {
        if self.free() >= need {
            return true;
        }
        if self.free() + usize::from(self.dead) < need {
            return false;
        }
        self.compact();
        true
    }

    /// Open slot `i` for an entry whose head is `head` and whose tail
    /// starts at `off`.
    fn put_slot(&mut self, i: usize, head: u64, off: usize) {
        let n = self.len();
        self.buf.copy_within(i * SLOT..n * SLOT, (i + 1) * SLOT);
        let s = i * SLOT;
        self.buf[s..s + 8].copy_from_slice(&head.to_be_bytes());
        self.buf[s + 8..s + 10].copy_from_slice(&(off as u16).to_le_bytes());
        self.n += 1;
    }

    /// Take entry `i` out, releasing any out-of-line slab.
    pub(crate) fn remove_at(&mut self, i: usize, ov: &mut Overflow) {
        let (bytes, slab) = self.tail_len(i);
        if let Some(id) = slab {
            ov.take(id);
        }
        self.drop_slots(i, i + 1, bytes);
    }

    /// Forget slots `from..to`, whose tails took `bytes`.
    fn drop_slots(&mut self, from: usize, to: usize, bytes: usize) {
        let n = self.len();
        self.buf.copy_within(to * SLOT..n * SLOT, from * SLOT);
        self.n -= (to - from) as u16;
        self.dead += bytes as u16;
        if self.n == 0 {
            self.top = BUF as u16;
            self.dead = 0;
        }
    }

    /// Rewrite the tails back to back, dropping dead bytes.
    fn compact(&mut self) {
        let old = self.buf;
        let mut top = BUF;
        for i in 0..self.len() {
            let start = self.off(i);
            // read from the copy: the page is being overwritten in place
            let (bytes, _) = tail_len_in(&old, start, self.shape);
            top -= bytes;
            self.buf[top..top + bytes].copy_from_slice(&old[start..start + bytes]);
            let s = i * SLOT + 8;
            self.buf[s..s + 2].copy_from_slice(&(top as u16).to_le_bytes());
        }
        self.top = top as u16;
        self.dead = 0;
    }

    /// Move entries `from..to` into `into` at slot `dst`, tail bytes as
    /// they are (an out-of-line slab moves with them); `into` must have room.
    pub(crate) fn move_span(&mut self, from: usize, to: usize, into: &mut Leaf, dst: usize) {
        let k = to - from;
        let bytes: usize = (from..to).map(|i| self.tail_len(i).0).sum();
        let fit = into.make_room(k * SLOT + bytes);
        debug_assert!(fit, "the receiving leaf has room");
        let n = into.len();
        into.buf.copy_within(dst * SLOT..n * SLOT, (dst + k) * SLOT);
        for (j, i) in (from..to).enumerate() {
            let (start, len) = (self.off(i), self.tail_len(i).0);
            let top = usize::from(into.top) - len;
            into.buf[top..top + len].copy_from_slice(&self.buf[start..start + len]);
            into.top = top as u16;
            let s = (dst + j) * SLOT;
            into.buf[s..s + 8].copy_from_slice(&self.head(i).to_be_bytes());
            into.buf[s + 8..s + 10].copy_from_slice(&(top as u16).to_le_bytes());
        }
        into.n += k as u16;
        self.drop_slots(from, to, bytes);
    }

    /// Remove the first `k` entries, releasing their slabs.
    pub(crate) fn remove_head(&mut self, k: usize, ov: &mut Overflow) {
        self.release_slabs(0, k, ov);
        let bytes = (0..k).map(|i| self.tail_len(i).0).sum();
        self.drop_slots(0, k, bytes);
    }

    /// Release every out-of-line slab entries `from..to` hold, before the
    /// leaf is dropped or they are copied away as owned bytes.
    pub(crate) fn release_slabs(&self, from: usize, to: usize, ov: &mut Overflow) {
        for i in from..to {
            if let (_, Some(id)) = self.tail_len(i) {
                ov.take(id);
            }
        }
    }

    /// Page bytes entries `from..to` take, slots included.
    pub(crate) fn span_bytes(&self, from: usize, to: usize) -> usize {
        (from..to).map(|i| SLOT + self.tail_len(i).0).sum()
    }

    /// Page bytes `e` would take here, slot included.
    pub(crate) fn span_of(&self, e: &Ent<'_>) -> usize {
        SLOT + self.tail_bytes(e.key.len(), e.payload.len(), e.vlen).0
    }

    /// The slot number of every entry's out-of-line slab, for checks.
    #[cfg(test)]
    pub(crate) fn slab_of(&self, i: usize) -> Option<u32> {
        self.tail_len(i).1
    }
}

#[cfg(test)]
#[path = "seg_leaf_tests.rs"]
mod tests;

//! Segmented list — element-granularity COW for giant lists.
//!
//! A list past [`SEG_PROMOTE`] elements stops being one `Arc<VecDeque>`
//! and becomes `Value::SegList(Arc<SegListData>)`: a deque of
//! [`SEG_CAP`]-element segments, each behind its own `Arc`. A snapshot
//! or rewrite view pins the whole structure by cloning the outer Arc
//! (which shares every segment); the first write during that window
//! clones the outer deque-of-Arcs (a pointer array — microseconds even
//! at hundreds of millions of elements) plus ONLY the segment it
//! touches, instead of the whole value. That turns the rc-soak's
//! multi-second `Arc::make_mut` reactor stall on multi-GB single lists
//! into a bounded ~one-segment clone (element-COW RFC under
//! the element-COW RFC).
//!
//! Lists at or below [`SEG_PROMOTE`] keep the flat `Value::List`
//! representation — the segment indirection is only paid where the
//! whole-value clone could hurt.
//!
//! ```
//! use kevy_store::ListData;
//! use kevy_store::list_seg::{SEG_PROMOTE, SegListData};
//! let flat: ListData = (0..SEG_PROMOTE + 1).map(|i| i.to_string().into_bytes()).collect();
//! let mut list = SegListData::from_flat(flat);
//! let snapshot = list.clone(); // shares every segment
//! list.push_back(b"tail".to_vec());
//! assert_eq!(snapshot.len(), SEG_PROMOTE + 1);
//! assert_eq!(list.len(), SEG_PROMOTE + 2);
//! ```

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::value::{ListData, list_item_weight};
use alloc::collections::VecDeque;
use alloc::sync::Arc;

/// Elements per segment. 16K × 64 B elements ≈ 1 MB — a COW clone of
/// one segment is ~1 ms worst-case (RFC Phase A: whole-value clone
/// measured ~50-70 ms per million elements; a segment caps the bound).
///
/// ```
/// use kevy_store::ListData;
/// use kevy_store::list_seg::{SEG_CAP, SegListData};
/// let flat: ListData = (0..SEG_CAP + 1).map(|_| b"x".to_vec()).collect();
/// let list = SegListData::from_flat(flat);
/// // the element past SEG_CAP starts a second segment; indexing stays global
/// assert_eq!(list.get(SEG_CAP).map(Vec::as_slice), Some(&b"x"[..]));
/// ```
pub const SEG_CAP: usize = 16 * 1024;
/// Flat `Value::List` length at which a push promotes to `SegList`.
///
/// ```
/// use kevy_store::Store;
/// use kevy_store::list_seg::SEG_PROMOTE;
/// let mut s = Store::new();
/// let items: Vec<&[u8]> = vec![b"v"; SEG_PROMOTE + 1];
/// assert_eq!(s.rpush(b"l", &items)?, SEG_PROMOTE + 1);
/// assert_eq!(s.llen(b"l")?, SEG_PROMOTE + 1);
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
pub const SEG_PROMOTE: usize = SEG_CAP;

/// A giant list: a deque of `Arc`-shared segments plus an O(1) length.
///
/// ```
/// use kevy_store::list_seg::SegListData;
/// let mut list = SegListData::default();
/// list.push_back(b"b".to_vec());
/// list.push_front(b"a".to_vec());
/// let all: Vec<&[u8]> = list.iter().map(Vec::as_slice).collect();
/// assert_eq!(all, [&b"a"[..], b"b"]);
/// ```
#[derive(Debug, Clone, Default)]
pub struct SegListData {
    segs: VecDeque<Arc<ListData>>,
    len: usize,
}

impl SegListData {
    #[inline]
    /// Elements across every segment, held as a running count rather than
    /// summed over the deque.
    ///
    /// ```
    /// use kevy_store::list_seg::SegListData;
    /// let list = SegListData::from_flat([b"a".to_vec(), b"b".to_vec()].into());
    /// assert_eq!(list.len(), 2);
    /// ```
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    /// Whether the list holds nothing.
    ///
    /// ```
    /// use kevy_store::list_seg::SegListData;
    /// let mut list = SegListData::default();
    /// assert!(list.is_empty());
    /// list.push_back(b"a".to_vec());
    /// assert!(!list.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Segment count — accounting walks charge per-segment overhead.
    #[inline]
    pub(crate) fn seg_count(&self) -> usize {
        self.segs.len()
    }

    /// Build from a flat list by moving its elements into segments.
    ///
    /// ```
    /// use kevy_store::ListData;
    /// use kevy_store::list_seg::SegListData;
    /// let flat: ListData = [b"a".to_vec(), b"b".to_vec()].into();
    /// let list = SegListData::from_flat(flat);
    /// assert_eq!(list.get(1).map(Vec::as_slice), Some(&b"b"[..]));
    /// ```
    pub fn from_flat(mut flat: ListData) -> Self {
        let mut out = SegListData::default();
        while !flat.is_empty() {
            let take = flat.len().min(SEG_CAP);
            let mut seg = ListData::with_capacity(take);
            seg.extend(flat.drain(..take));
            out.len += seg.len();
            out.segs.push_back(Arc::new(seg));
        }
        out
    }

    /// Prepend. Touches the first segment only, so under a snapshot the
    /// copy-on-write clone is one segment plus the pointer deque — not the
    /// list. A full front segment is not split; a new one is pushed ahead
    /// of it.
    ///
    /// ```
    /// use kevy_store::list_seg::SegListData;
    /// let mut list = SegListData::default();
    /// list.push_front(b"b".to_vec());
    /// list.push_front(b"a".to_vec());
    /// assert_eq!(list.get(0).map(Vec::as_slice), Some(&b"a"[..]));
    /// ```
    pub fn push_front(&mut self, v: Vec<u8>) {
        match self.segs.front_mut() {
            Some(s) if s.len() < SEG_CAP => Arc::make_mut(s).push_front(v),
            _ => {
                let mut seg = ListData::with_capacity(1);
                seg.push_back(v);
                self.segs.push_front(Arc::new(seg));
            }
        }
        self.len += 1;
    }

    /// Append, on the same one-segment terms as `push_front`.
    ///
    /// ```
    /// use kevy_store::list_seg::SegListData;
    /// let mut list = SegListData::default();
    /// list.push_back(b"a".to_vec());
    /// list.push_back(b"b".to_vec());
    /// assert_eq!(list.get(1).map(Vec::as_slice), Some(&b"b"[..]));
    /// ```
    pub fn push_back(&mut self, v: Vec<u8>) {
        match self.segs.back_mut() {
            Some(s) if s.len() < SEG_CAP => Arc::make_mut(s).push_back(v),
            _ => {
                let mut seg = ListData::with_capacity(1);
                seg.push_back(v);
                self.segs.push_back(Arc::new(seg));
            }
        }
        self.len += 1;
    }

    /// Take from the front. An emptied segment is dropped rather than
    /// kept, so a list that is drained does not keep its segment array.
    ///
    /// ```
    /// use kevy_store::list_seg::SegListData;
    /// let mut list = SegListData::from_flat([b"a".to_vec(), b"b".to_vec()].into());
    /// assert_eq!(list.pop_front(), Some(b"a".to_vec()));
    /// assert_eq!(list.len(), 1);
    /// ```
    pub fn pop_front(&mut self) -> Option<Vec<u8>> {
        let seg = self.segs.front_mut()?;
        let v = Arc::make_mut(seg).pop_front()?;
        if seg.is_empty() {
            self.segs.pop_front();
        }
        self.len -= 1;
        Some(v)
    }

    /// Take from the back, on the same terms as `pop_front`.
    ///
    /// ```
    /// use kevy_store::list_seg::SegListData;
    /// let mut list = SegListData::from_flat([b"a".to_vec(), b"b".to_vec()].into());
    /// assert_eq!(list.pop_back(), Some(b"b".to_vec()));
    /// assert_eq!(list.pop_back(), Some(b"a".to_vec()));
    /// assert_eq!(list.pop_back(), None);
    /// ```
    pub fn pop_back(&mut self) -> Option<Vec<u8>> {
        let seg = self.segs.back_mut()?;
        let v = Arc::make_mut(seg).pop_back()?;
        if seg.is_empty() {
            self.segs.pop_back();
        }
        self.len -= 1;
        Some(v)
    }

    /// Segment index + in-segment offset for a global element index.
    /// Caller guarantees `idx < self.len`.
    fn locate(&self, idx: usize) -> (usize, usize) {
        let mut remaining = idx;
        for (si, seg) in self.segs.iter().enumerate() {
            if remaining < seg.len() {
                return (si, remaining);
            }
            remaining -= seg.len();
        }
        unreachable!("locate past end");
    }

    /// Index into the list. Walks the segment deque to find the one that
    /// contains `idx`, so this is O(segments) rather than O(1) — cheap at
    /// the sizes segmentation is for, since a segment holds `SEG_CAP`
    /// elements.
    ///
    /// ```
    /// use kevy_store::list_seg::SegListData;
    /// let list = SegListData::from_flat([b"a".to_vec(), b"b".to_vec()].into());
    /// assert_eq!(list.get(0).map(Vec::as_slice), Some(&b"a"[..]));
    /// assert_eq!(list.get(2), None);
    /// ```
    pub fn get(&self, idx: usize) -> Option<&Vec<u8>> {
        if idx >= self.len {
            return None;
        }
        let (si, off) = self.locate(idx);
        self.segs[si].get(off)
    }

    /// Replace the element at `idx`; returns the old element. COW cost:
    /// the hit segment only.
    ///
    /// ```
    /// use kevy_store::list_seg::SegListData;
    /// let mut list = SegListData::from_flat([b"a".to_vec(), b"b".to_vec()].into());
    /// assert_eq!(list.set(1, b"B".to_vec()), b"b".to_vec());
    /// assert_eq!(list.get(1).map(Vec::as_slice), Some(&b"B"[..]));
    /// ```
    pub fn set(&mut self, idx: usize, val: Vec<u8>) -> Vec<u8> {
        let (si, off) = self.locate(idx);
        core::mem::replace(&mut Arc::make_mut(&mut self.segs[si])[off], val)
    }

    /// Insert at global `idx` (may equal `len` = append). A segment
    /// grown past `SEG_CAP` by the insert is split in half so repeated
    /// inserts can't re-create the unbounded-clone problem.
    ///
    /// ```
    /// use kevy_store::list_seg::SegListData;
    /// let mut list = SegListData::from_flat([b"a".to_vec(), b"c".to_vec()].into());
    /// list.insert(1, b"b".to_vec());
    /// let all: Vec<&[u8]> = list.iter().map(Vec::as_slice).collect();
    /// assert_eq!(all, [&b"a"[..], b"b", b"c"]);
    /// ```
    pub fn insert(&mut self, idx: usize, val: Vec<u8>) {
        if idx >= self.len {
            self.push_back(val);
            return;
        }
        let (si, off) = self.locate(idx);
        let seg = Arc::make_mut(&mut self.segs[si]);
        seg.insert(off, val);
        self.len += 1;
        if seg.len() > SEG_CAP {
            let tail = seg.split_off(seg.len() / 2);
            self.segs.insert(si + 1, Arc::new(tail));
        }
    }

    /// Global index of the first element equal to `val`.
    ///
    /// ```
    /// use kevy_store::list_seg::SegListData;
    /// let list = SegListData::from_flat([b"a".to_vec(), b"b".to_vec()].into());
    /// assert_eq!(list.position(b"b"), Some(1));
    /// assert_eq!(list.position(b"z"), None);
    /// ```
    pub fn position(&self, val: &[u8]) -> Option<usize> {
        let mut base = 0;
        for seg in &self.segs {
            if let Some(i) = seg.iter().position(|v| v.as_slice() == val) {
                return Some(base + i);
            }
            base += seg.len();
        }
        None
    }

    /// Every element front to back, flattening the segments.
    ///
    /// ```
    /// use kevy_store::list_seg::SegListData;
    /// let list = SegListData::from_flat([b"a".to_vec(), b"b".to_vec()].into());
    /// let all: Vec<&[u8]> = list.iter().map(Vec::as_slice).collect();
    /// assert_eq!(all, [&b"a"[..], b"b"]);
    /// ```
    pub fn iter(&self) -> impl Iterator<Item = &Vec<u8>> {
        self.segs.iter().flat_map(|s| s.iter())
    }

    /// Iterate `count` elements starting at global `start` — seeks to
    /// the segment in O(segments) instead of skip-walking elements.
    ///
    /// ```
    /// use kevy_store::list_seg::SegListData;
    /// let list = SegListData::from_flat([b"a".to_vec(), b"b".to_vec(), b"c".to_vec()].into());
    /// let tail: Vec<&[u8]> = list.range(1, 2).map(Vec::as_slice).collect();
    /// assert_eq!(tail, [&b"b"[..], b"c"]);
    /// ```
    pub fn range(&self, start: usize, count: usize) -> impl Iterator<Item = &Vec<u8>> {
        let (si, off) = if start >= self.len { (self.segs.len(), 0) } else { self.locate(start) };
        self.segs.iter().skip(si).flat_map(|s| s.iter()).skip(off).take(count)
    }

    /// `LREM` walk: remove up to `|count|` occurrences of `val`
    /// (`count >= 0` head-first, `< 0` tail-first, `0` = all). Only
    /// segments containing a match are COW-cloned. Returns
    /// `(removed, weight_delta)`.
    ///
    /// ```
    /// use kevy_store::list_item_weight;
    /// use kevy_store::list_seg::SegListData;
    /// let flat = [b"x".to_vec(), b"y".to_vec(), b"x".to_vec()].into();
    /// let mut list = SegListData::from_flat(flat);
    /// let (removed, delta) = list.remove_occurrences(b"x", -1);
    /// assert_eq!(removed, 1);
    /// assert_eq!(delta, -(list_item_weight(1) as i64));
    /// assert_eq!(list.position(b"x"), Some(0)); // tail-first: the head one survives
    /// ```
    pub fn remove_occurrences(&mut self, val: &[u8], count: i64) -> (usize, i64) {
        let limit = match count {
            0 => usize::MAX,
            c if c > 0 => c as usize,
            c => (-c) as usize,
        };
        let (mut removed, mut delta) = (0usize, 0i64);
        let indices: Vec<usize> = (0..self.segs.len()).collect();
        let order: Vec<usize> =
            if count >= 0 { indices } else { indices.into_iter().rev().collect() };
        for si in order {
            if removed >= limit {
                break;
            }
            if !self.segs[si].iter().any(|v| v.as_slice() == val) {
                continue;
            }
            let seg = Arc::make_mut(&mut self.segs[si]);
            if count >= 0 {
                let mut i = 0;
                while i < seg.len() && removed < limit {
                    if seg[i].as_slice() == val {
                        delta -= list_item_weight(seg[i].len()) as i64;
                        seg.remove(i);
                        removed += 1;
                    } else {
                        i += 1;
                    }
                }
            } else {
                let mut i = seg.len();
                while i > 0 && removed < limit {
                    i -= 1;
                    if seg[i].as_slice() == val {
                        delta -= list_item_weight(seg[i].len()) as i64;
                        seg.remove(i);
                        removed += 1;
                    }
                }
            }
        }
        self.len -= removed;
        self.segs.retain(|s| !s.is_empty());
        (removed, delta)
    }

    /// `LTRIM` to keep `[start, stop]` (inclusive, already normalised).
    /// Whole segments outside the range are dropped WITHOUT cloning —
    /// their elements are walked read-only for the weight delta, then
    /// the segment Arc is released. Returns the (negative) weight delta.
    ///
    /// ```
    /// use kevy_store::list_seg::SegListData;
    /// let flat = [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()].into();
    /// let mut list = SegListData::from_flat(flat);
    /// assert!(list.trim_to(1, 1) < 0);
    /// let kept: Vec<&[u8]> = list.iter().map(Vec::as_slice).collect();
    /// assert_eq!(kept, [&b"b"[..]]);
    /// ```
    pub fn trim_to(&mut self, start: usize, stop: usize) -> i64 {
        let mut delta = 0i64;
        // Drop from the front: whole segments below `start`.
        let mut to_skip = start;
        while let Some(front) = self.segs.front() {
            if front.len() <= to_skip {
                to_skip -= front.len();
                self.len -= front.len();
                delta -= front.iter().map(|v| list_item_weight(v.len()) as i64).sum::<i64>();
                self.segs.pop_front();
            } else {
                break;
            }
        }
        if to_skip > 0
            && let Some(front) = self.segs.front_mut()
        {
            let seg = Arc::make_mut(front);
            for v in seg.drain(..to_skip) {
                delta -= list_item_weight(v.len()) as i64;
            }
            self.len -= to_skip;
        }
        // Drop from the back: everything past the (shifted) stop.
        let keep = stop - start + 1;
        while self.len > keep {
            let over = self.len - keep;
            let back = self.segs.back_mut().expect("len > keep implies segments");
            if back.len() <= over {
                self.len -= back.len();
                delta -= back.iter().map(|v| list_item_weight(v.len()) as i64).sum::<i64>();
                self.segs.pop_back();
            } else {
                let seg = Arc::make_mut(back);
                let cut = seg.len() - over;
                for v in seg.drain(cut..) {
                    delta -= list_item_weight(v.len()) as i64;
                }
                self.len -= over;
            }
        }
        delta
    }

    /// Empty the list, returning the (negative) weight delta. Read-only
    /// walk for accounting; shared segments are released, not cloned.
    ///
    /// ```
    /// use kevy_store::list_seg::SegListData;
    /// let mut list = SegListData::from_flat([b"a".to_vec()].into());
    /// assert!(list.clear() < 0);
    /// assert!(list.is_empty());
    /// ```
    pub fn clear(&mut self) -> i64 {
        let delta = -(self.iter().map(|v| list_item_weight(v.len()) as i64).sum::<i64>());
        self.segs.clear();
        self.len = 0;
        delta
    }

    /// Test-only segment introspection — COW tests assert which
    /// segments a write actually cloned.
    #[cfg(test)]
    pub(crate) fn seg_arcs(&self) -> impl Iterator<Item = &Arc<ListData>> {
        self.segs.iter()
    }

    /// Are the outer structure and every segment uniquely owned? The
    /// bio-drop gate: only a fully-unique SegList really frees its
    /// payload on drop (shared segments would just decrement).
    pub(crate) fn all_unique(&self) -> bool {
        self.segs.iter().all(|s| Arc::strong_count(s) == 1)
    }
}

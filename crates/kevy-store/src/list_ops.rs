//! `Store` list ops introduced in v1.27.3 for BullMQ end-to-end:
//! `RPOPLPUSH`, `LMOVE`, `LPOS`. Kept in a sibling module to keep
//! `list.rs` under the 500-LOC house rule.
//!
//! All three are local-shard-only for v1.27.3 — the cross-shard
//! Take→Put orchestrator (mirroring `RENAME`'s `exec_rename`) is a
//! later runtime concern; the dispatch layer routes by source key and
//! these helpers operate on whatever the local `Store` holds for `dst`.

use crate::value::Value;
use crate::{Store, StoreError};

impl Store {
    /// `RPOPLPUSH source destination` — atomically pop one element from
    /// the tail of `src` and push it onto the head of `dst`. Returns the
    /// moved element, or `None` if `src` was empty / absent.
    ///
    /// When `src == dst` Redis defines the result as a rotation
    /// (tail → head of the same list), which falls out of this code
    /// naturally because the pop sees the pre-rotation tail.
    pub fn rpoplpush(
        &mut self,
        src: &[u8],
        dst: &[u8],
    ) -> Result<Option<Vec<u8>>, StoreError> {
        // WRONGTYPE pre-check on dst: if dst exists but isn't a list,
        // we must reject BEFORE consuming the src element (Redis: the
        // pop is reverted on WRONGTYPE at the destination).
        match self.live_entry(dst) {
            None => {}
            Some(e) => match &e.value {
                Value::List(_) | Value::SmallListInline(_) => {}
                _ => return Err(StoreError::WrongType),
            },
        }
        let mut popped = self.rpop(src, 1)?;
        let Some(v) = popped.pop() else {
            return Ok(None);
        };
        // Push to the head of dst. `lpush_borrowed` returns the new
        // length; we want the popped value back to the caller.
        self.lpush_borrowed(dst, &[v.as_slice()])?;
        Ok(Some(v))
    }

    /// `LMOVE source destination LEFT|RIGHT LEFT|RIGHT` — generalised
    /// `RPOPLPUSH`. `from_left=true` pops from the head, otherwise the
    /// tail; `to_left=true` pushes to the head, otherwise the tail.
    pub fn lmove(
        &mut self,
        src: &[u8],
        dst: &[u8],
        from_left: bool,
        to_left: bool,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        match self.live_entry(dst) {
            None => {}
            Some(e) => match &e.value {
                Value::List(_) | Value::SmallListInline(_) => {}
                _ => return Err(StoreError::WrongType),
            },
        }
        let mut popped = if from_left {
            self.lpop(src, 1)?
        } else {
            self.rpop(src, 1)?
        };
        let Some(v) = popped.pop() else {
            return Ok(None);
        };
        if to_left {
            self.lpush_borrowed(dst, &[v.as_slice()])?;
        } else {
            self.rpush_borrowed(dst, &[v.as_slice()])?;
        }
        Ok(Some(v))
    }

    /// `LPOS key element [RANK n] [COUNT n] [MAXLEN n]` — find the
    /// zero-based position(s) of `element` in the list.
    ///
    /// * `rank > 0` — scan head→tail, skipping the first `rank-1`
    ///   matches. `rank == 1` (default) returns the first match.
    /// * `rank < 0` — scan tail→head, returning matches as
    ///   absolute (head-relative) indices.
    /// * `count` — `None` returns the first match as a 1-element vec
    ///   (caller emits an integer / nil); `Some(0)` returns all
    ///   matches; `Some(n)` caps to `n`.
    /// * `maxlen` — `0` means unlimited; otherwise stop after
    ///   scanning that many elements (in the chosen direction).
    ///
    /// Returns the matched indices in scan order. An empty result with
    /// `count == None` is the caller's signal to emit RESP nil.
    pub fn lpos(
        &mut self,
        key: &[u8],
        element: &[u8],
        rank: i64,
        count: Option<i64>,
        maxlen: usize,
    ) -> Result<Vec<i64>, StoreError> {
        if rank == 0 {
            return Err(StoreError::OutOfRange);
        }
        if let Some(c) = count {
            if c < 0 {
                return Err(StoreError::OutOfRange);
            }
        }
        let entries: Vec<Vec<u8>> = match self.live_entry(key) {
            None => return Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::List(l) => l.iter().cloned().collect(),
                Value::SmallListInline(l) => l.iter().map(<[u8]>::to_vec).collect(),
                _ => return Err(StoreError::WrongType),
            },
        };
        let n = entries.len();
        if n == 0 {
            return Ok(Vec::new());
        }
        let skip = (rank.unsigned_abs() as usize).saturating_sub(1);
        let cap = match count {
            None => 1,
            Some(0) => usize::MAX,
            Some(c) => c as usize,
        };
        let want_reverse = rank < 0;
        let scan_limit = if maxlen == 0 { n } else { maxlen.min(n) };
        let mut out = Vec::new();
        let mut scanned = 0usize;
        let mut skipped = 0usize;
        let iter: Box<dyn Iterator<Item = (usize, &Vec<u8>)>> = if want_reverse {
            Box::new(entries.iter().enumerate().rev())
        } else {
            Box::new(entries.iter().enumerate())
        };
        for (idx, v) in iter {
            if scanned >= scan_limit {
                break;
            }
            scanned += 1;
            if v.as_slice() == element {
                if skipped < skip {
                    skipped += 1;
                    continue;
                }
                out.push(idx as i64);
                if out.len() >= cap {
                    break;
                }
            }
        }
        Ok(out)
    }
}

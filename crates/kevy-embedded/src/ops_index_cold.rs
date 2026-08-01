//! The cold-aware halves of the scalar query surface: the windowed
//! per-shard iterator and the range/count entry points that merge a
//! windowed index's cold segments into the hot tree's answer. Split
//! from `ops_index.rs` for the 500-LOC house rule.

use kevy_index::{Cursor, IndexSpec, IndexValue, Segment};

use crate::ops_index::{IndexPage, WinRef, merge_page, sync_segs};
use crate::store::{Store, lock_write};
use crate::{KevyError, KevyResult};

impl Store {
    /// Range / EQ query with cursor pagination: merged across shards
    /// in `(value, key)` order — including any cold segments a
    /// windowed index has slid out. `cursor = None` starts; the
    /// returned cursor resumes exclusively.
    pub fn idx_query(
        &self,
        name: &[u8],
        min: &IndexValue,
        max: &IndexValue,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> KevyResult<IndexPage> {
        let limit = limit.clamp(1, 100_000);
        let mut all: Vec<(IndexValue, Vec<u8>)> = Vec::new();
        self.for_each_segment_windowed(name, |spec, seg, win| {
            let (hits, _) = seg.range(min, max, cursor, limit);
            all.extend(hits.into_iter().map(|(k, v)| (v, k)));
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(w) = win.filter(|w| w.has_cold()) {
                let cold = w
                    .cold_hits(spec.ty, min, max, limit)
                    .map_err(|e| KevyError::Io(std::io::Error::other(e)))?;
                let after = |k: &[u8], v: &IndexValue| match cursor {
                    None => true,
                    Some(c) => (v, k) > (&c.value, c.key.as_slice()),
                };
                all.extend(cold.into_iter().filter(|(k, v)| after(k, v)).map(|(k, v)| (v, k)));
            }
            #[cfg(target_arch = "wasm32")]
            let _ = (spec, win);
            Ok(())
        })?;
        Ok(merge_page(all, limit))
    }

    /// Count without materializing keys — hot tree plus cold segments.
    pub fn idx_count(&self, name: &[u8], min: &IndexValue, max: &IndexValue) -> KevyResult<u64> {
        let mut total = 0u64;
        self.for_each_segment_windowed(name, |spec, seg, win| {
            total += seg.count(min, max);
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(w) = win.filter(|w| w.has_cold()) {
                total += w
                    .cold_count(spec.ty, min, max)
                    .map_err(|e| KevyError::Io(std::io::Error::other(e)))?;
            }
            #[cfg(target_arch = "wasm32")]
            let _ = (spec, win);
            Ok(())
        })?;
        Ok(total)
    }


    /// [`Self::for_each_segment`], with each shard's window runtime
    /// (if any) beside the segment, and a fallible visitor — the cold
    /// half does I/O, and a corrupt cold segment must become the
    /// query's error, never a partial answer.
    pub(crate) fn for_each_segment_windowed(
        &self,
        name: &[u8],
        mut f: impl FnMut(&IndexSpec, &Segment, WinRef<'_>) -> KevyResult<()>,
    ) -> KevyResult<()> {
        let mut found = false;
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            let segs = &inner.idx_segs;
            if let Some((spec, seg)) = segs.segs.iter().find(|(s, _)| s.name == name) {
                found = true;
                #[cfg(not(target_arch = "wasm32"))]
                let win = segs.window_of(name);
                #[cfg(target_arch = "wasm32")]
                let win = None;
                f(spec, seg, win)?;
            }
        }
        if found {
            Ok(())
        } else {
            Err(KevyError::NotFound("no such index".into()))
        }
    }

}

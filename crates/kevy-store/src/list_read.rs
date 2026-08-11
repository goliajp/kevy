//! `Store` list read commands (LLEN / LINDEX / LRANGE) — split from
//! `list.rs` when the SegList arms pushed it against the 500-LOC cap.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::util::{norm_index, range_bounds};
use crate::value::Value;
use crate::{Store, StoreError};

impl Store {
    pub fn llen(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        match self.live_entry(key) {
            None => Ok(0),
            Some(e) => match &e.value {
                Value::List(l) => Ok(l.len()),
                Value::SegList(l) => Ok(l.len()),
                Value::SmallListInline(l) => Ok(l.len()),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    pub fn lindex(&mut self, key: &[u8], idx: i64) -> Result<Option<Vec<u8>>, StoreError> {
        match self.live_entry(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::List(l) => Ok(norm_index(idx, l.len()).and_then(|i| l.get(i).cloned())),
                Value::SegList(l) => {
                    Ok(norm_index(idx, l.len()).and_then(|i| l.get(i).cloned()))
                }
                Value::SmallListInline(l) => {
                    let n = l.len();
                    let Some(i) = norm_index(idx, n) else { return Ok(None) };
                    Ok(l.iter().nth(i).map(<[u8]>::to_vec))
                }
                _ => Err(StoreError::WrongType),
            },
        }
    }

    pub fn lrange(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        match self.live_entry(key) {
            None => Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::List(l) => Ok(match range_bounds(start, stop, l.len()) {
                    None => Vec::new(),
                    Some((s, end)) => l.iter().skip(s).take(end - s + 1).cloned().collect(),
                }),
                // Seeks to the start segment instead of skip-walking
                // elements — an LRANGE deep into a giant list stays
                // O(segments + span).
                Value::SegList(l) => Ok(match range_bounds(start, stop, l.len()) {
                    None => Vec::new(),
                    Some((s, end)) => l.iter_range(s, end - s + 1).cloned().collect(),
                }),
                Value::SmallListInline(l) => Ok(match range_bounds(start, stop, l.len()) {
                    None => Vec::new(),
                    Some((s, end)) => l
                        .iter()
                        .skip(s)
                        .take(end - s + 1)
                        .map(<[u8]>::to_vec)
                        .collect(),
                }),
                _ => Err(StoreError::WrongType),
            },
        }
    }
}

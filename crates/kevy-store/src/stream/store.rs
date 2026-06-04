//! `Store::xadd` / `xlen` / `xrange` / `xrevrange` / `xread` / `xdel` /
//! `xtrim_*` — the per-keyspace surface for sprint A of v2-7 Streams.
//! Kept separate from `stream/mod.rs` (which owns the `StreamData` /
//! `StreamId` types + entry-side ops) so each file stays under the
//! project's ≤500-LOC rule.

use super::{StreamData, StreamId, XAddIdSpec};
use crate::value::*;
use crate::{Entry, Store, StoreError};

/// Cloned-out view of stream entries, the cross-module wire form. Keeps
/// the same shape Redis sends and lets the callers stay decoupled from
/// the `SmallBytes` interning the store uses internally.
pub type EntryBatch = Vec<(StreamId, Vec<(Vec<u8>, Vec<u8>)>)>;

impl Store {
    fn stream_mut(
        &mut self,
        key: &[u8],
        create: bool,
    ) -> Result<Option<&mut StreamData>, StoreError> {
        if self.live_entry_mut(key).is_none() {
            if !create {
                return Ok(None);
            }
            self.insert_entry(
                SmallBytes::from_slice(key),
                Entry::new(Value::Stream(Box::default()), None),
            );
        }
        match &mut self.map.get_mut(key).expect("present").value {
            Value::Stream(s) => Ok(Some(s)),
            _ => Err(StoreError::WrongType),
        }
    }

    fn stream_ref(&mut self, key: &[u8]) -> Result<Option<&StreamData>, StoreError> {
        match self.live_entry(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::Stream(s) => Ok(Some(s.as_ref())),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// `XADD key <spec> field value [field value ...]`. Returns the
    /// assigned ID. `nomkstream` matches Redis's `NOMKSTREAM` flag —
    /// suppress key creation, returning `Ok(None)`. `now_ms` is the
    /// wall-clock used for `XAddIdSpec::AutoAll`.
    pub fn xadd(
        &mut self,
        key: &[u8],
        spec: XAddIdSpec,
        fields: Vec<(Vec<u8>, Vec<u8>)>,
        nomkstream: bool,
        now_ms: u64,
    ) -> Result<Option<StreamId>, StoreError> {
        if nomkstream && self.live_entry(key).is_none() {
            return Ok(None);
        }
        let id;
        let weight_delta;
        {
            let s = self.stream_mut(key, true)?.expect("created");
            id = s.resolve_xadd_id(spec, now_ms)?;
            let smb_fields: Vec<(SmallBytes, SmallBytes)> = fields
                .into_iter()
                .map(|(f, v)| (SmallBytes::from_slice(&f), SmallBytes::from_slice(&v)))
                .collect();
            weight_delta = super::stream_entry_weight(&smb_fields);
            s.insert(id, smb_fields);
        }
        self.bump_if_watched(key);
        self.account_delta(key, weight_delta as i64);
        Ok(Some(id))
    }

    /// `XLEN key`. Returns 0 for a missing key.
    pub fn xlen(&mut self, key: &[u8]) -> Result<u64, StoreError> {
        Ok(self.stream_ref(key)?.map_or(0, |s| s.length()))
    }

    /// `XRANGE key start end [COUNT n]`.
    pub fn xrange(
        &mut self,
        key: &[u8],
        start: StreamId,
        end: StreamId,
        count: Option<usize>,
    ) -> Result<EntryBatch, StoreError> {
        Ok(self
            .stream_ref(key)?
            .map_or_else(Vec::new, |s| super::clone_entries(s.range(start, end, count))))
    }

    /// `XREVRANGE key end start [COUNT n]`.
    pub fn xrevrange(
        &mut self,
        key: &[u8],
        start: StreamId,
        end: StreamId,
        count: Option<usize>,
    ) -> Result<EntryBatch, StoreError> {
        Ok(self
            .stream_ref(key)?
            .map_or_else(Vec::new, |s| super::clone_entries(s.revrange(start, end, count))))
    }

    /// `XREAD ... STREAMS key last_seen [...]` — per-key part.
    pub fn xread(
        &mut self,
        key: &[u8],
        last_seen: StreamId,
        count: Option<usize>,
    ) -> Result<EntryBatch, StoreError> {
        Ok(self
            .stream_ref(key)?
            .map_or_else(Vec::new, |s| super::clone_entries(s.read_after(last_seen, count))))
    }

    /// Resolve `$` as XREAD's "last-seen" to the stream's current last
    /// ID. Returns `MIN` for a missing key.
    pub fn xread_dollar_last_id(&mut self, key: &[u8]) -> Result<StreamId, StoreError> {
        Ok(self.stream_ref(key)?.map_or(StreamId::MIN, |s| s.last_id()))
    }

    /// `XDEL key id [...]`. Returns count actually removed.
    pub fn xdel(&mut self, key: &[u8], ids: &[StreamId]) -> Result<u64, StoreError> {
        let n;
        {
            let Some(s) = self.stream_mut(key, false)? else {
                return Ok(0);
            };
            n = s.del_ids(ids);
        }
        if n > 0 {
            self.bump_if_watched(key);
            self.reweigh_entry(key);
        }
        Ok(n as u64)
    }

    /// `XTRIM key MAXLEN n`. Returns number removed.
    pub fn xtrim_maxlen(&mut self, key: &[u8], maxlen: u64) -> Result<u64, StoreError> {
        let n;
        {
            let Some(s) = self.stream_mut(key, false)? else {
                return Ok(0);
            };
            n = s.trim_maxlen(maxlen as usize);
        }
        if n > 0 {
            self.bump_if_watched(key);
            self.reweigh_entry(key);
        }
        Ok(n as u64)
    }

    /// `XTRIM key MINID id`. Returns number removed.
    pub fn xtrim_minid(&mut self, key: &[u8], minid: StreamId) -> Result<u64, StoreError> {
        let n;
        {
            let Some(s) = self.stream_mut(key, false)? else {
                return Ok(0);
            };
            n = s.trim_minid(minid);
        }
        if n > 0 {
            self.bump_if_watched(key);
            self.reweigh_entry(key);
        }
        Ok(n as u64)
    }
}

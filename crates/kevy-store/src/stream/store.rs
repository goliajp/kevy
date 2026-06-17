//! `Store::xadd` / `xlen` / `xrange` / `xrevrange` / `xread` / `xdel` /
//! `xtrim_*` — the per-keyspace surface for sprint A of v2-7 Streams.
//! Kept separate from `stream/mod.rs` (which owns the `StreamData` /
//! `StreamId` types + entry-side ops) so each file stays under the
//! project's ≤500-LOC rule.

use super::group::{AutoclaimResult, ReadGroupId};
use super::{
    GroupCreateMode, PendingExtended, PendingSummary, StreamData, StreamId, XAddIdSpec, XClaimOpts,
};
use crate::value::{SmallBytes, Value};
use crate::{Entry, Store, StoreError};
use std::sync::Arc;

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
                Entry::new(Value::Stream(Arc::default()), None),
            );
        }
        match &mut self.map.get_mut(key).expect("present").value {
            Value::Stream(s) => Ok(Some(Arc::make_mut(s))),
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

    /// Read-only access to a stream's `StreamData`, used by `XINFO`
    /// to inspect entries / groups / consumers without going through
    /// the wrapper layer. Returns `Ok(None)` for a missing key,
    /// `WrongType` for a non-stream value at `key`.
    pub fn stream_view(&mut self, key: &[u8]) -> Result<Option<&StreamData>, StoreError> {
        self.stream_ref(key)
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
        Ok(self.stream_ref(key)?.map_or(0, super::StreamData::length))
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
        Ok(self.stream_ref(key)?.map_or(StreamId::MIN, super::StreamData::last_id))
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

    /// `XSETID key last-id [ENTRIESADDED n] [MAXDELETEDID id]`. Returns
    /// `NoSuchKey` for a missing key (dispatch maps it to Redis's
    /// "requires the key to exist" wording), `OutOfRange` when `last_id`
    /// is below the stream's top entry.
    pub fn xsetid(
        &mut self,
        key: &[u8],
        last_id: StreamId,
        entries_added: Option<u64>,
        max_deleted_id: Option<StreamId>,
    ) -> Result<(), StoreError> {
        {
            let Some(s) = self.stream_mut(key, false)? else {
                return Err(StoreError::NoSuchKey);
            };
            s.xsetid(last_id, entries_added, max_deleted_id)?;
        }
        self.bump_if_watched(key);
        Ok(())
    }

    // ─────── consumer-group surface (sprint B) ───────

    /// `XGROUP CREATE key group <id|$> [MKSTREAM]`. Returns `Ok(true)`
    /// when a fresh group was added; `Ok(false)` if the group already
    /// existed (caller emits `-BUSYGROUP`). `mkstream` matches Redis:
    /// auto-create the stream key when missing.
    pub fn xgroup_create(
        &mut self,
        key: &[u8],
        group: &[u8],
        mode: GroupCreateMode,
        mkstream: bool,
    ) -> Result<bool, StoreError> {
        let exists = self.live_entry(key).is_some();
        if !exists && !mkstream {
            return Err(StoreError::NoSuchKey);
        }
        let s = self.stream_mut(key, true)?.expect("created");
        let created = s.group_create(group, mode)?;
        self.bump_if_watched(key);
        self.reweigh_entry(key);
        Ok(created)
    }

    /// `XGROUP DESTROY key group`. Returns `true` if a group was dropped.
    pub fn xgroup_destroy(&mut self, key: &[u8], group: &[u8]) -> Result<bool, StoreError> {
        let dropped;
        {
            let Some(s) = self.stream_mut(key, false)? else {
                return Ok(false);
            };
            dropped = s.group_destroy(group);
        }
        if dropped {
            self.bump_if_watched(key);
            self.reweigh_entry(key);
        }
        Ok(dropped)
    }

    /// `XGROUP SETID key group <id|$>`.
    pub fn xgroup_setid(
        &mut self,
        key: &[u8],
        group: &[u8],
        mode: GroupCreateMode,
    ) -> Result<bool, StoreError> {
        let touched;
        {
            let Some(s) = self.stream_mut(key, false)? else {
                return Ok(false);
            };
            touched = s.group_setid(group, mode);
        }
        if touched {
            self.bump_if_watched(key);
        }
        Ok(touched)
    }

    /// `XGROUP CREATECONSUMER key group consumer`.
    pub fn xgroup_create_consumer(
        &mut self,
        key: &[u8],
        group: &[u8],
        consumer: &[u8],
        now_ms: u64,
    ) -> Result<bool, StoreError> {
        let Some(s) = self.stream_mut(key, false)? else {
            return Ok(false);
        };
        Ok(s.group_create_consumer(group, consumer, now_ms))
    }

    /// `XGROUP DELCONSUMER key group consumer`. Returns dropped PEL count.
    pub fn xgroup_del_consumer(
        &mut self,
        key: &[u8],
        group: &[u8],
        consumer: &[u8],
    ) -> Result<u64, StoreError> {
        let Some(s) = self.stream_mut(key, false)? else {
            return Ok(0);
        };
        Ok(s.group_del_consumer(group, consumer))
    }

    /// `XREADGROUP GROUP g c [COUNT n] [NOACK] STREAMS key id`.
    #[allow(clippy::too_many_arguments)]
    pub fn xreadgroup(
        &mut self,
        key: &[u8],
        group: &[u8],
        consumer: &[u8],
        last_seen: ReadGroupId,
        count: Option<usize>,
        noack: bool,
        now_ms: u64,
    ) -> Result<EntryBatch, StoreError> {
        let result;
        {
            let Some(s) = self.stream_mut(key, false)? else {
                return Err(StoreError::NoSuchKey);
            };
            result = s.readgroup(group, consumer, last_seen, count, noack, now_ms)?;
        }
        if !result.is_empty() {
            self.bump_if_watched(key);
        }
        Ok(result)
    }

    /// Non-destructive: would `XREADGROUP … STREAMS key >` yield new
    /// entries for `group` right now? True iff the stream's last id is
    /// past the group's last-delivered id. Used by the cross-shard BLOCK
    /// arbiter's readiness peek — never advances the group cursor. False
    /// for a missing key / group.
    pub fn xreadgroup_has_new(&mut self, key: &[u8], group: &[u8]) -> Result<bool, StoreError> {
        Ok(self
            .stream_ref(key)?
            .and_then(|s| s.group(group).map(|g| s.last_id() > g.last_delivered_id()))
            .unwrap_or(false))
    }

    /// `XACK key group id [id ...]`. Returns count of PEL removals.
    pub fn xack(&mut self, key: &[u8], group: &[u8], ids: &[StreamId]) -> Result<u64, StoreError> {
        let n;
        {
            let Some(s) = self.stream_mut(key, false)? else {
                return Ok(0);
            };
            n = s.ack(group, ids);
        }
        if n > 0 {
            self.bump_if_watched(key);
        }
        Ok(n)
    }

    /// `XPENDING key group` — summary form.
    pub fn xpending_summary(
        &mut self,
        key: &[u8],
        group: &[u8],
    ) -> Result<Option<PendingSummary>, StoreError> {
        Ok(self.stream_ref(key)?.and_then(|s| s.pending_summary(group)))
    }

    /// `XPENDING key group [IDLE ms] start end count [consumer]` —
    /// extended form.
    #[allow(clippy::too_many_arguments)]
    pub fn xpending_extended(
        &mut self,
        key: &[u8],
        group: &[u8],
        idle_min_ms: Option<u64>,
        start: StreamId,
        end: StreamId,
        count: usize,
        consumer_filter: Option<&[u8]>,
        now_ms: u64,
    ) -> Result<Option<PendingExtended>, StoreError> {
        Ok(self.stream_ref(key)?.and_then(|s| {
            s.pending_extended(group, idle_min_ms, start, end, count, consumer_filter, now_ms)
        }))
    }

    /// `XCLAIM key group consumer min-idle-ms id [id ...] [...]`.
    /// Returns the (id, field-value) pairs successfully claimed —
    /// dispatcher trims to ID-only when `JUSTID` is set.
    pub fn xclaim(
        &mut self,
        key: &[u8],
        group: &[u8],
        new_owner: &[u8],
        ids: &[StreamId],
        opts: &XClaimOpts,
        now_ms: u64,
    ) -> Result<EntryBatch, StoreError> {
        let claimed;
        let payloads;
        {
            let Some(s) = self.stream_mut(key, false)? else {
                return Err(StoreError::NoSuchKey);
            };
            claimed = s.claim(group, new_owner, ids, opts, now_ms)?;
            payloads = s.payloads_for(&claimed);
        }
        if !claimed.is_empty() {
            self.bump_if_watched(key);
        }
        Ok(payloads)
    }

    /// `XAUTOCLAIM key group consumer min-idle-ms start [COUNT n]
    /// [JUSTID]`. Returns the cursor + claimed payloads + deleted IDs.
    #[allow(clippy::too_many_arguments)]
    pub fn xautoclaim(
        &mut self,
        key: &[u8],
        group: &[u8],
        new_owner: &[u8],
        min_idle_ms: u64,
        start: StreamId,
        count: usize,
        justid: bool,
        now_ms: u64,
    ) -> Result<(StreamId, EntryBatch, Vec<StreamId>), StoreError> {
        let payloads;
        let next_cursor;
        let deleted_ids;
        {
            let Some(s) = self.stream_mut(key, false)? else {
                return Err(StoreError::NoSuchKey);
            };
            let AutoclaimResult { next_cursor: nc, claimed_ids, deleted_ids: di } =
                s.autoclaim(group, new_owner, min_idle_ms, start, count, justid, now_ms)?;
            payloads = s.payloads_for(&claimed_ids);
            next_cursor = nc;
            deleted_ids = di;
        }
        if !payloads.is_empty() {
            self.bump_if_watched(key);
        }
        Ok((next_cursor, payloads, deleted_ids))
    }
}

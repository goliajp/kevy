//! Snapshot / replication / reshard load hooks on [`Store`] — the
//! `load_value` re-home dispatch and the stream loader. Split out of
//! `keyspace.rs` (500-LOC house rule); behaviour unchanged.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;

use alloc::sync::Arc;

use crate::value::Value;
use crate::{SmallBytes, Store};

impl Store {
    /// Insert one already-typed `(key, value, ttl)` triple, e.g. straight out
    /// of another store's [`Self::snapshot_each`] — the redistribution step
    /// both reshard paths (embedded `shards` bring-up, server routing
    /// migration) use to re-home keys after a layout change.
    // LOC-WAIVER: pure per-Value-variant dispatch table — one arm per
    // stored type routing it to its typed loader; no control flow.
    pub fn load_value(&mut self, key: &[u8], value: &Value, ttl_ms: Option<u64>) {
        let k = key.to_vec();
        match value {
            Value::Str(v) => self.load_str(k, v.to_vec(), ttl_ms),
            // L2: snapshot/replication load keeps the encoding — store as
            // Int directly to preserve the in-memory shape (and avoid the
            // SET-detect parse on the load path).
            Value::Int(n) => self.insert_loaded(k, Value::Int(*n), ttl_ms),
            // L1: preserve the Arc-backed encoding on snapshot/replication
            // load. Arc::clone is cheap; avoids re-copying the bytes.
            Value::ArcBulk(a) => self.insert_loaded(k, Value::ArcBulk(a.clone()), ttl_ms),
            Value::Hash(h) => {
                self.load_hash(k, h.iter().map(|(f, v)| (f.to_vec(), v.clone())).collect(), ttl_ms)
            }
            // A.8: same shape as A.7 — re-materialise to the heap-backed
            // variant on snapshot/replication load. First mutation that
            // targets the key will go through the encoding-switch path
            // and (if size still fits) re-promote to the inline variant.
            Value::SmallHashInline(h) => {
                self.load_hash(k, h.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect(), ttl_ms)
            }
            Value::List(l) => self.load_list(k, l.iter().cloned().collect(), ttl_ms),
            Value::SmallListInline(l) => {
                self.load_list(k, l.iter().map(<[u8]>::to_vec).collect(), ttl_ms)
            }
            Value::Set(s) => {
                self.load_set(k, s.iter().map(kevy_bytes::SmallBytes::to_vec).collect(), ttl_ms)
            }
            // A.7 O5: snapshot/replication load — re-materialise the
            // inline-encoded set as a `Value::Set`-backed `KevySet`. We
            // don't preserve the SmallSetInline encoding on reload
            // because (a) the upgrade path will naturally rebuild it on
            // the first SADD that targets the key, and (b) the snapshot
            // wire format already uses the OP_SET length-prefixed
            // payload — losing the inline encoding bit costs nothing
            // beyond one re-promotion on the first mutation.
            Value::SmallSetInline(s) => {
                self.load_set(k, s.iter_slices().map(<[u8]>::to_vec).collect(), ttl_ms)
            }
            Value::ZSet(z) => {
                self.load_zset(k, z.ordered().map(|(m, sc)| (m.to_vec(), sc)).collect(), ttl_ms)
            }
            Value::SmallZSetInline(z) => {
                self.load_zset(k, z.iter().map(|(m, sc)| (m.to_vec(), sc)).collect(), ttl_ms)
            }
            Value::Stream(st) => self.load_stream_value(k, st, ttl_ms),
            // Unreachable by construction: every producer of a shipped
            // value (take_with_ttl / clone_with_ttl / snapshot_each
            // consumers) materializes cold values on ITS side —
            // a ColdRef names the source shard's vlog, which this store
            // cannot read. Skip rather than alias a foreign record;
            // loud in debug builds.
            Value::Cold(_) => debug_assert!(
                false,
                "load_value received a cold stub — source must materialize before shipping"
            ),
        }
    }

    /// [`Self::load_value`]'s stream arm: decode the live `StreamData`
    /// into the primitive tuples [`Self::load_stream`] takes.
    fn load_stream_value(&mut self, k: Vec<u8>, st: &crate::StreamData, ttl_ms: Option<u64>) {
        let entries: Vec<crate::stream::LoadedStreamEntry> = st
            .iter_entries()
            .map(|(id, fv)| {
                let fvv = fv
                    .iter()
                    .map(|(f, v)| (f.as_slice().to_vec(), v.as_slice().to_vec()))
                    .collect();
                (id.ms, id.seq, fvv)
            })
            .collect();
        let last = st.last_id();
        let mxd = st.max_deleted_id();
        self.load_stream(
            k,
            entries,
            (last.ms, last.seq),
            (mxd.ms, mxd.seq),
            st.entries_added(),
            st.export_groups(),
            ttl_ms,
        );
    }

    /// Snapshot-load a stream: every entry plus the per-stream scalar
    /// state (last_id, max_deleted_id, entries_added) and the consumer
    /// groups are restored verbatim. Caller passes already-decoded
    /// primitive tuples; this fn does the [`SmallBytes`] /
    /// [`crate::StreamData`] conversion.
    #[allow(clippy::too_many_arguments)]
    pub fn load_stream(
        &mut self,
        key: Vec<u8>,
        entries: Vec<crate::stream::LoadedStreamEntry>,
        last_id: (u64, u64),
        max_deleted_id: (u64, u64),
        entries_added: u64,
        groups: Vec<crate::stream::LoadedGroup>,
        ttl_ms: Option<u64>,
    ) {
        let mut s = crate::stream::StreamData::default();
        for (ms, seq, fv) in entries {
            let id = crate::stream::StreamId { ms, seq };
            let fv_small: Vec<(SmallBytes, SmallBytes)> = fv
                .into_iter()
                .map(|(f, v)| (SmallBytes::from_vec(f), SmallBytes::from_vec(v)))
                .collect();
            s.load_entry(id, fv_small);
        }
        s.set_loaded_state(
            crate::stream::StreamId { ms: last_id.0, seq: last_id.1 },
            crate::stream::StreamId { ms: max_deleted_id.0, seq: max_deleted_id.1 },
            entries_added,
        );
        s.import_groups(groups);
        self.insert_loaded(key, Value::Stream(Arc::new(s)), ttl_ms);
    }
}

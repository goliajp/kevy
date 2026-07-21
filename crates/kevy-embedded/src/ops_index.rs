//! Embedded secondary-index API (server parity
//! minus FIELDS hydration, which exists to save wire round-trips the
//! embedded caller doesn't have — read fields with `hget` directly).
//!
//! Placement: each shard's `Inner` carries its slice of every index
//! (index-follows-key, same as the server), maintained inside
//! `commit_write` under the shard lock the write already holds — the
//! synchronous-derivation guarantee costs no extra locking. Key
//! extraction from the logged argv is EXACT (a precise multi-key
//! table, not the feed filter's fail-open heuristic): a missed update
//! would be index drift, which derived-by-construction forbids.
//!
//! Backfill: `idx_create` builds synchronously, shard by shard, each
//! under its own write lock (the hook holds the same lock, so there is
//! no race window per shard). No `Building` state embedded — create
//! returns when the index serves.

use crate::{KevyError, KevyResult};
use std::io;
use std::sync::RwLock;

use kevy_index::{Catalog, Cursor, IndexKind, IndexSpec, IndexValue, Segment, SegmentStats, ValType};

use crate::store::{Store, lock_write};

pub(crate) use crate::ops_index_sync::{each_written_key_pub, on_commit, sync_segs};

/// One page of index hits plus the cursor to resume from.
pub type IndexPage = (Vec<(Vec<u8>, IndexValue)>, Option<Cursor>);

/// One field's highlight: its name and the `(start, end)` match spans.
#[cfg(feature = "text")]
pub type FieldSpans = (Vec<u8>, Vec<(u32, u32)>);
/// A highlighted MATCH hit: key, score, and per-field [`FieldSpans`].
#[cfg(feature = "text")]
pub type HighlightedHit = (Vec<u8>, f64, Vec<FieldSpans>);

// `idx_match_highlighted` and its span-mapping helper live in a child
// module to keep this file under the 500-LOC ceiling.
#[cfg(feature = "text")]
#[path = "ops_index_highlight.rs"]
mod highlight;

/// Sort merged `(value, key)` hits, cut to `limit`, and derive the
/// resume cursor. Shared by `Store::idx_query` and the transaction twin
/// on `AtomicAllShards`, which differ only in where the segments come
/// from — the pagination has to agree exactly or a cursor taken inside
/// a transaction would not resume outside one.
pub(crate) fn merge_page(mut all: Vec<(IndexValue, Vec<u8>)>, limit: usize) -> IndexPage {
    all.sort();
    all.truncate(limit);
    let next = if all.len() == limit {
        all.last().map(|(v, k)| Cursor { value: v.clone(), key: k.clone() })
    } else {
        None
    };
    (all.into_iter().map(|(v, k)| (k, v)).collect(), next)
}

/// Store-level index state: catalog + a version stamp the per-shard
/// segment lists sync against.
#[derive(Default)]
pub(crate) struct IndexReg {
    pub(crate) catalog: RwLock<(u64, Catalog)>,
}

/// Per-shard segment list, kept inside `Inner` (guarded by the shard
/// lock).
#[derive(Default)]
pub(crate) struct ShardSegs {
    pub(crate) version: u64,
    pub(crate) segs: Vec<(IndexSpec, Segment)>,
    /// Inverted segments for KIND text specs (parallel list —
    /// a spec appears in exactly one of the lists).
    #[cfg(feature = "text")]
    pub(crate) text: Vec<(IndexSpec, kevy_text::TextSegment)>,
    /// HNSW graphs for KIND ann specs.
    #[cfg(feature = "vector")]
    pub(crate) ann: Vec<(IndexSpec, kevy_vector::Hnsw)>,
    /// Aggregate segments for KIND agg specs.
    pub(crate) agg: Vec<(IndexSpec, kevy_index::AggSegment)>,
}

#[cfg(feature = "persist")]
const SIDECAR: &str = "index-catalog.meta";

impl Store {
    /// `IDX.CREATE` equivalent. Builds synchronously; errors on
    /// duplicate name / cap / bad spec.
    pub fn idx_create(
        &self,
        name: &[u8],
        prefix: &[u8],
        field: &[u8],
        ty: ValType,
        kind: IndexKind,
    ) -> KevyResult<()> {
        if prefix.is_empty() {
            return Err(KevyError::InvalidInput("empty prefix".into()));
        }
        #[cfg(not(feature = "text"))]
        if kind == IndexKind::Text {
            return Err(KevyError::Unsupported("text indexes need the `text` feature".into()));
        }
        #[cfg(not(feature = "vector"))]
        if kind == IndexKind::Ann {
            return Err(KevyError::Unsupported("vector indexes need the `vector` feature".into()));
        }
        let spec = IndexSpec {
            name: name.to_vec(),
            prefix: prefix.to_vec(),
            fields: vec![kevy_index::FieldSpec::new(field.to_vec())],
            ty,
            kind,
            max_bytes: 0,
            ann: None,
            group_by: None,
            with_positions: false,
        };
        self.register_spec(spec)
    }

    fn register_spec(&self, spec: IndexSpec) -> KevyResult<()> {
        {
            let mut g = self
                .indexes
                .catalog
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (ver, cat) = &mut *g;
            cat.create(spec)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            *ver += 1;
        }
        self.persist_index_sidecar();
        // Build every shard's slice now (each under its own lock).
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
        }
        Ok(())
    }

    /// Declare an ANN index (KIND ann, TYPE vector). `params.m`
    /// / `params.ef` of 0 select the defaults (16 / 200).
    #[cfg(feature = "vector")]
    pub fn idx_create_ann(
        &self,
        name: &[u8],
        prefix: &[u8],
        field: &[u8],
        params: kevy_index::AnnSpec,
    ) -> KevyResult<()> {
        if params.dim == 0 || params.distance > 2 {
            return Err(KevyError::InvalidInput("bad ann parameters".into()));
        }
        let spec = IndexSpec {
            name: name.to_vec(),
            prefix: prefix.to_vec(),
            fields: vec![kevy_index::FieldSpec::new(field.to_vec())],
            ty: ValType::Vector,
            kind: IndexKind::Ann,
            max_bytes: 0,
            ann: Some(kevy_index::AnnSpec {
                m: if params.m == 0 { 16 } else { params.m },
                ef: if params.ef == 0 { 200 } else { params.ef },
                ..params
            }),
            group_by: None,
            with_positions: false,
        };
        self.register_spec(spec)
    }

    /// `IDX.DROP` equivalent; `false` if absent. On a hit the catalog
    /// sidecar is re-persisted so the drop survives restart.
    pub fn idx_drop(&self, name: &[u8]) -> bool {
        let hit = {
            let mut g = self
                .indexes
                .catalog
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (ver, cat) = &mut *g;
            let hit = cat.drop_index(name);
            if hit {
                *ver += 1;
            }
            hit
        };
        if hit {
            self.persist_index_sidecar();
        }
        hit
    }

    /// Range / EQ query with cursor pagination: merged across shards
    /// in `(value, key)` order. `cursor = None` starts; the returned
    /// cursor resumes exclusively.
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
        self.for_each_segment(name, |seg| {
            let (hits, _) = seg.range(min, max, cursor, limit);
            all.extend(hits.into_iter().map(|(k, v)| (v, k)));
        })?;
        Ok(merge_page(all, limit))
    }

    /// Count without materializing keys.
    pub fn idx_count(&self, name: &[u8], min: &IndexValue, max: &IndexValue) -> KevyResult<u64> {
        let mut total = 0u64;
        self.for_each_segment(name, |seg| total += seg.count(min, max))?;
        Ok(total)
    }

    /// Summed segment stats (entries / bytes / coerce failures /
    /// unique-fence duplicates).
    pub fn idx_stats(&self, name: &[u8]) -> KevyResult<SegmentStats> {
        let mut sum = SegmentStats::default();
        self.for_each_segment(name, |seg| {
            let s = seg.stats();
            sum.entries += s.entries;
            sum.approx_bytes += s.approx_bytes;
            sum.coerce_failures += s.coerce_failures;
            sum.duplicates += s.duplicates;
        })?;
        Ok(sum)
    }

    /// Declared indexes (name, prefix, kind), declaration order.
    pub fn idx_list(&self) -> Vec<(Vec<u8>, Vec<u8>, IndexKind)> {
        let g = self
            .indexes
            .catalog
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.1.iter()
            .map(|(s, _)| (s.name.clone(), s.prefix.clone(), s.kind))
            .collect()
    }

    /// `MATCH` — BM25-ranked hits merged across shards, scored against
    /// **global** corpus statistics so a hit's rank does not depend on
    /// which shard it landed on (see docs/text-search.md).
    ///
    /// Two query-time passes: the first sums each shard's `n_docs`,
    /// `total_len` and per-query-token `df` into one [`CorpusStats`]; the
    /// second scores every shard against it. Only the query's tokens'
    /// df is aggregated, not a whole-corpus table — the query narrows it.
    #[cfg(feature = "text")]
    pub fn idx_match(
        &self,
        name: &[u8],
        query: &[u8],
        limit: usize,
    ) -> KevyResult<Vec<(Vec<u8>, f64)>> {
        Ok(self
            .idx_match_highlighted(name, query, limit, None)?
            .into_iter()
            .map(|(key, score, _)| (key, score))
            .collect())
    }

    /// Pass 1 of `idx_match`: sum each shard's corpus counters and the
    /// df of every query token into one global [`CorpusStats`]. Errors
    /// if no shard carries a text index named `name`.
    #[cfg(feature = "text")]
    fn text_corpus_stats(&self, name: &[u8], text: &[u8]) -> KevyResult<kevy_text::CorpusStats> {
        let (mut n_docs, mut total_len) = (0f64, 0u64);
        // Accumulated per shard from `query_df_terms`, which expands
        // `word*` prefixes against that shard's dictionary — so the df map
        // ends up keyed by the union of every shard's query terms and
        // prefix expansions, each summed to a global df.
        let mut df: std::collections::HashMap<Vec<u8>, u32> = std::collections::HashMap::new();
        let mut found = false;
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            if let Some((_, ts)) = inner.idx_segs.text.iter().find(|(s, _)| s.name == name) {
                found = true;
                n_docs += ts.stats().docs as f64;
                total_len += ts.total_len();
                for t in ts.query_df_terms(text) {
                    let d = ts.local_df(&t);
                    *df.entry(t).or_insert(0) += d;
                }
            }
        }
        if !found {
            return Err(KevyError::NotFound("no such text index".into()));
        }
        let avgdl = if n_docs > 0.0 { total_len as f64 / n_docs } else { 0.0 };
        Ok(kevy_text::CorpusStats { n_docs, avgdl, df })
    }

    /// Declare an aggregate index (KIND agg — write-time GROUP
    /// BY). `ty` must be numeric.
    pub fn idx_create_agg(
        &self,
        name: &[u8],
        prefix: &[u8],
        field: &[u8],
        ty: ValType,
        group_by: &[u8],
    ) -> KevyResult<()> {
        if !matches!(ty, ValType::I64 | ValType::F64) || group_by.is_empty() {
            return Err(KevyError::InvalidInput("agg requires numeric type + group field".into()));
        }
        let spec = IndexSpec {
            name: name.to_vec(),
            prefix: prefix.to_vec(),
            fields: vec![kevy_index::FieldSpec::new(field.to_vec())],
            ty,
            kind: IndexKind::Agg,
            max_bytes: 0,
            ann: None,
            group_by: Some(group_by.to_vec()),
            with_positions: false,
        };
        self.register_spec(spec)
    }

    /// One group's merged stats across shards.
    pub fn idx_group(&self, name: &[u8], group: &[u8]) -> KevyResult<kevy_index::GroupStats> {
        let mut merged = kevy_index::GroupStats { count: 0, sum: 0.0, min: None, max: None };
        let mut found = false;
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            if let Some((_, a)) = inner.idx_segs.agg.iter().find(|(s, _)| s.name == name) {
                found = true;
                kevy_index::merge_group(&mut merged, &a.group(group));
            }
        }
        if !found {
            return Err(KevyError::NotFound("no such aggregate index".into()));
        }
        Ok(merged)
    }

    /// Top groups merged + ranked across shards.
    pub fn idx_groups(
        &self,
        name: &[u8],
        by: kevy_index::AggBy,
        limit: usize,
    ) -> KevyResult<Vec<(Vec<u8>, kevy_index::GroupStats)>> {
        let limit = limit.clamp(1, 1000);
        // HashMap merge (same O(rows×groups) trap the server reduce
        // had — hashing keeps it linear).
        let mut merged: std::collections::HashMap<Vec<u8>, kevy_index::GroupStats> =
            std::collections::HashMap::new();
        let mut found = false;
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            if let Some((_, a)) = inner.idx_segs.agg.iter().find(|(s, _)| s.name == name) {
                found = true;
                for (gk, st) in a.all_groups() {
                    match merged.get_mut(&gk) {
                        Some(m) => kevy_index::merge_group(m, &st),
                        None => {
                            merged.insert(gk, st);
                        }
                    }
                }
            }
        }
        if !found {
            return Err(KevyError::NotFound("no such aggregate index".into()));
        }
        let mut ranked: Vec<(Vec<u8>, kevy_index::GroupStats)> = merged.into_iter().collect();
        kevy_index::sort_groups(&mut ranked, by);
        ranked.truncate(limit);
        Ok(ranked)
    }

    /// `KNN` — nearest neighbors merged ascending across shards.
    /// `ef` = query beam width (0 = engine default; recall knob).
    #[cfg(feature = "vector")]
    pub fn idx_knn(
        &self,
        name: &[u8],
        query: &[f32],
        k: usize,
        ef: usize,
    ) -> KevyResult<Vec<(Vec<u8>, f32)>> {
        let k = k.clamp(1, 1000);
        let mut all: Vec<(Vec<u8>, f32)> = Vec::new();
        let mut found = false;
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            if let Some((_, graph)) = inner.idx_segs.ann.iter().find(|(s, _)| s.name == name) {
                found = true;
                all.extend(graph.knn(query, k, ef));
            }
        }
        if !found {
            return Err(KevyError::NotFound("no such vector index".into()));
        }
        all.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        all.truncate(k);
        Ok(all)
    }

    /// Without `persist` there is no data dir — the catalog lives only
    /// in memory, so the sidecar halves are no-ops.
    #[cfg(not(feature = "persist"))]
    fn persist_index_sidecar(&self) {}

    #[cfg(not(feature = "persist"))]
    pub(crate) fn idx_boot(&self) {}

    fn for_each_segment(
        &self,
        name: &[u8],
        mut f: impl FnMut(&Segment),
    ) -> KevyResult<()> {
        let mut found = false;
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            if let Some((_, seg)) = inner.idx_segs.segs.iter().find(|(s, _)| s.name == name) {
                found = true;
                f(seg);
            }
        }
        if found {
            Ok(())
        } else {
            Err(KevyError::NotFound("no such index".into()))
        }
    }

    #[cfg(feature = "persist")]
    fn persist_index_sidecar(&self) {
        let Some(dir) = &self.config.data_dir else { return };
        let g = self
            .indexes
            .catalog
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = dir.join("index-catalog.meta.tmp");
        if std::fs::write(&tmp, g.1.to_sidecar()).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join(SIDECAR));
        }
    }

    /// Boot half — load a persisted catalog (indexes rebuild lazily on
    /// first touch via `sync_segs`).
    #[cfg(feature = "persist")]
    pub(crate) fn idx_boot(&self) {
        let Some(dir) = &self.config.data_dir else { return };
        if let Ok(text) = std::fs::read_to_string(dir.join(SIDECAR))
            && let Some(cat) = Catalog::from_sidecar(&text)
            && !cat.is_empty()
        {
            let mut g = self
                .indexes
                .catalog
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *g = (g.0 + 1, cat);
        }
    }
}

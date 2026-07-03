//! v2.5 — embedded secondary-index API (RFC LOCKED; server parity
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

use std::io;
use std::sync::RwLock;

use kevy_index::{Catalog, Cursor, IndexKind, IndexSpec, IndexValue, Segment, SegmentStats, ValType};

use crate::store::{Store, lock_write};

/// One page of index hits plus the cursor to resume from.
pub type IndexPage = (Vec<(Vec<u8>, IndexValue)>, Option<Cursor>);

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
    /// v2.7: inverted segments for KIND text specs (parallel list —
    /// a spec appears in exactly one of the two).
    pub(crate) text: Vec<(IndexSpec, kevy_text::TextSegment)>,
}

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
    ) -> io::Result<()> {
        if prefix.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty prefix"));
        }
        let spec = IndexSpec {
            name: name.to_vec(),
            prefix: prefix.to_vec(),
            field: field.to_vec(),
            ty,
            kind,
            max_bytes: 0,
        };
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

    /// `IDX.DROP` equivalent; `false` if absent.
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
    ) -> io::Result<IndexPage> {
        let limit = limit.clamp(1, 100_000);
        let mut all: Vec<(IndexValue, Vec<u8>)> = Vec::new();
        self.for_each_segment(name, |seg| {
            let (hits, _) = seg.range(min, max, cursor, limit);
            all.extend(hits.into_iter().map(|(k, v)| (v, k)));
        })?;
        all.sort();
        all.truncate(limit);
        let next = if all.len() == limit {
            all.last().map(|(v, k)| Cursor { value: v.clone(), key: k.clone() })
        } else {
            None
        };
        Ok((all.into_iter().map(|(v, k)| (k, v)).collect(), next))
    }

    /// Count without materializing keys.
    pub fn idx_count(&self, name: &[u8], min: &IndexValue, max: &IndexValue) -> io::Result<u64> {
        let mut total = 0u64;
        self.for_each_segment(name, |seg| total += seg.count(min, max))?;
        Ok(total)
    }

    /// Summed segment stats (entries / bytes / coerce failures /
    /// unique-fence duplicates).
    pub fn idx_stats(&self, name: &[u8]) -> io::Result<SegmentStats> {
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

    /// v2.7 `MATCH` — BM25-ranked hits merged across shards
    /// (shard-local statistics; see docs/text-search.md).
    pub fn idx_match(
        &self,
        name: &[u8],
        query: &[u8],
        limit: usize,
    ) -> io::Result<Vec<(Vec<u8>, f64)>> {
        let limit = limit.clamp(1, 1000);
        let mut all: Vec<kevy_text::TextMatch> = Vec::new();
        let mut found = false;
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            if let Some((_, ts)) = inner.idx_segs.text.iter().find(|(s, _)| s.name == name) {
                found = true;
                all.extend(ts.matches(query, limit));
            }
        }
        if !found {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such text index"));
        }
        all.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.key.cmp(&b.key)));
        all.truncate(limit);
        Ok(all.into_iter().map(|m| (m.key, m.score)).collect())
    }

    fn for_each_segment(
        &self,
        name: &[u8],
        mut f: impl FnMut(&Segment),
    ) -> io::Result<()> {
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
            Err(io::Error::new(io::ErrorKind::NotFound, "no such index"))
        }
    }

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

/// Reconcile one shard's segment list with the catalog; new indexes
/// backfill from this shard's live keys (we hold the shard's write
/// lock — no concurrent writes can race the scan).
pub(crate) fn sync_segs(
    reg: &IndexReg,
    shard_segs: &mut ShardSegs,
    store: &mut kevy_store::Store,
) {
    let g = reg
        .catalog
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (ver, cat) = &*g;
    if shard_segs.version == *ver {
        return;
    }
    let mut next: Vec<(IndexSpec, Segment)> = Vec::new();
    let mut next_text: Vec<(IndexSpec, kevy_text::TextSegment)> = Vec::new();
    for (spec, _) in cat.iter() {
        if spec.kind == kevy_index::IndexKind::Text {
            match shard_segs.text.iter().position(|(s, _)| s == spec) {
                Some(i) => next_text.push(shard_segs.text.swap_remove(i)),
                None => {
                    let mut ts = kevy_text::TextSegment::new();
                    let mut pat = spec.prefix.clone();
                    pat.push(b'*');
                    for key in store.collect_keys(Some(&pat), None) {
                        apply_text_key(store, spec, &mut ts, &key);
                    }
                    next_text.push((spec.clone(), ts));
                }
            }
            continue;
        }
        match shard_segs.segs.iter().position(|(s, _)| s == spec) {
            Some(i) => next.push(shard_segs.segs.swap_remove(i)),
            None => {
                let mut seg = Segment::new();
                let mut pat = spec.prefix.clone();
                pat.push(b'*');
                for key in store.collect_keys(Some(&pat), None) {
                    apply_key(store, spec, &mut seg, &key);
                }
                next.push((spec.clone(), seg));
            }
        }
    }
    shard_segs.segs = next;
    shard_segs.text = next_text;
    shard_segs.version = *ver;
}

fn apply_text_key(
    store: &mut kevy_store::Store,
    spec: &IndexSpec,
    ts: &mut kevy_text::TextSegment,
    key: &[u8],
) {
    match store.hget(key, &spec.field) {
        Ok(Some(raw)) => {
            let raw = raw.to_vec();
            ts.apply(key, Some(&raw));
        }
        _ => ts.apply(key, None),
    }
}

/// The write-path hook body — called from `commit_write` with the
/// logged argv, under the shard lock. Extracts the written key(s)
/// EXACTLY and re-derives their index entries.
pub(crate) fn on_commit(
    reg: &IndexReg,
    shard_segs: &mut ShardSegs,
    store: &mut kevy_store::Store,
    parts: &[&[u8]],
) {
    {
        // Cheap gate: empty catalog = one read-lock + is_empty.
        let g = reg
            .catalog
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if g.1.is_empty() {
            return;
        }
    }
    sync_segs(reg, shard_segs, store);
    let verb = parts.first().copied().unwrap_or(b"");
    if verb.eq_ignore_ascii_case(b"FLUSHALL") || verb.eq_ignore_ascii_case(b"FLUSHDB") {
        for (_, seg) in &mut shard_segs.segs {
            *seg = Segment::new();
        }
        for (_, ts) in &mut shard_segs.text {
            *ts = kevy_text::TextSegment::new();
        }
        return;
    }
    each_written_key(verb, parts, |key| {
        for (spec, seg) in &mut shard_segs.segs {
            if key.starts_with(&spec.prefix) {
                apply_key(store, spec, seg, key);
            }
        }
        for (spec, ts) in &mut shard_segs.text {
            if key.starts_with(&spec.prefix) {
                apply_text_key(store, spec, ts, key);
            }
        }
    });
}

/// EXACT written-key walk per verb shape (the logged effect argv is a
/// closed set — new effect verbs must be added here; the parity test
/// in `store_tests_index.rs` pins the list).
pub(crate) fn each_written_key_pub(verb: &[u8], parts: &[&[u8]], f: impl FnMut(&[u8])) {
    each_written_key(verb, parts, f);
}

fn each_written_key(verb: &[u8], parts: &[&[u8]], mut f: impl FnMut(&[u8])) {
    let up = |v: &[u8], t: &[u8]| v.eq_ignore_ascii_case(t);
    if up(verb, b"DEL") || up(verb, b"UNLINK") {
        for k in &parts[1..] {
            f(k);
        }
    } else if up(verb, b"MSET") {
        let mut i = 1;
        while i + 1 < parts.len() {
            f(parts[i]);
            i += 2;
        }
    } else if up(verb, b"COPY") || up(verb, b"RENAME") || up(verb, b"RENAMENX") {
        if let Some(k) = parts.get(1) {
            f(k);
        }
        if let Some(k) = parts.get(2) {
            f(k);
        }
    } else if let Some(k) = parts.get(1) {
        f(k);
    }
}

fn apply_key(store: &mut kevy_store::Store, spec: &IndexSpec, seg: &mut Segment, key: &[u8]) {
    match store.hget(key, &spec.field) {
        Ok(Some(raw)) => {
            let raw = raw.to_vec();
            match IndexValue::coerce(spec.ty, &raw) {
                Some(v) => seg.apply(key, Some(v)),
                None => seg.apply(key, None),
            }
        }
        Ok(None) => {
            if store.exists(&[key.to_vec()]) == 0 {
                seg.remove(key);
            } else {
                seg.apply(key, None);
            }
        }
        Err(_) => seg.remove(key),
    }
}

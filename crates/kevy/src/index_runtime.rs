//! v2.5 — the index engine's runtime half (RFC LOCKED 2026-07-04).
//!
//! Topology: one process-global [`Catalog`] behind an RwLock +
//! generation counter; each shard thread keeps a thread-local
//! [`ShardIndexes`] (its slice of every index — index-follows-key)
//! refreshed lazily when the generation moves. The write path enters
//! through [`on_write`] (wired to `Commands::on_write`), whose first
//! instruction is a process-wide `NONEMPTY` Relaxed load — the RFC D2
//! zero-tax gate: an empty catalog costs one untaken branch.
//!
//! Backfill (RFC D5, tick-incremental variant): `IDX.CREATE` snapshots
//! the domain's key list per shard; `on_shard_tick` indexes a bounded
//! batch per tick until exhausted (non-blocking, no extra threads,
//! shard-affine). Live writes during the build hit the hook first and
//! win: the backfill only fills keys the segment doesn't hold yet, so
//! a newer hook-applied value is never clobbered by a stale scan.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use kevy_index::{Catalog, IndexSpec, IndexValue, Segment};
use kevy_store::Store;

static NONEMPTY: AtomicBool = AtomicBool::new(false);
static CATALOG_GEN: AtomicU64 = AtomicU64::new(0);
static CATALOG: RwLock<Option<Arc<Catalog>>> = RwLock::new(None);

/// Per-shard build progress for one index.
enum BuildState {
    /// Keys captured at create-time, next position to process.
    Backfilling { keys: Vec<Vec<u8>>, pos: usize },
    /// Serving.
    Ready,
}

struct ShardIndex {
    spec: IndexSpec,
    seg: Segment,
    build: BuildState,
}

#[derive(Default)]
struct ShardIndexes {
    generation: u64,
    idx: Vec<ShardIndex>,
}

thread_local! {
    static SHARD_INDEXES: RefCell<ShardIndexes> = RefCell::new(ShardIndexes::default());
}

/// Snapshot the current catalog (None = empty).
pub(crate) fn catalog() -> Option<Arc<Catalog>> {
    CATALOG
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Swap in a new catalog version (IDX.CREATE / IDX.DROP). Bumps the
/// generation; shards refresh lazily.
pub(crate) fn install_catalog(c: Catalog) {
    let nonempty = !c.is_empty();
    *CATALOG
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(c));
    NONEMPTY.store(nonempty, Ordering::Release);
    CATALOG_GEN.fetch_add(1, Ordering::Release);
}

/// The write-path hook body (`Commands::on_write`).
#[inline]
pub(crate) fn on_write(store: &mut Store, key: &[u8]) {
    if !NONEMPTY.load(Ordering::Relaxed) {
        return;
    }
    SHARD_INDEXES.with(|tl| {
        let mut st = tl.borrow_mut();
        refresh(&mut st, store);
        for si in &mut st.idx {
            if key.starts_with(&si.spec.prefix) {
                apply_row(store, si, key);
            }
        }
    });
}

/// Tick hook: advance backfills a bounded batch per tick.
pub(crate) fn on_tick(store: &mut Store) {
    if !NONEMPTY.load(Ordering::Relaxed) {
        return;
    }
    SHARD_INDEXES.with(|tl| {
        let mut st = tl.borrow_mut();
        refresh(&mut st, store);
        for si in &mut st.idx {
            advance_backfill(store, si, 2048);
        }
    });
}

/// Query entry: run `f` against this shard's segment for `name`.
/// `None` = index unknown here (stale TL is refreshed first) or still
/// backfilling. Wired to IDX.QUERY fan-out in step 2b.
pub(crate) fn with_ready_segment<R>(
    store: &mut Store,
    name: &[u8],
    f: impl FnOnce(&IndexSpec, &Segment) -> R,
) -> Result<R, &'static str> {
    SHARD_INDEXES.with(|tl| {
        let mut st = tl.borrow_mut();
        refresh(&mut st, store);
        let si = st
            .idx
            .iter()
            .find(|si| si.spec.name == name)
            .ok_or("ERR no such index")?;
        match si.build {
            BuildState::Ready => Ok(f(&si.spec, &si.seg)),
            BuildState::Backfilling { .. } => Err("INDEXBUILDING index is still building"),
        }
    })
}

/// Two-segment variant for COMPOSE — one RefCell borrow (nesting
/// [`with_ready_segment`] would double-borrow the thread-local).
pub(crate) fn with_two_ready_segments<R>(
    store: &mut Store,
    a: &[u8],
    b: &[u8],
    f: impl FnOnce(&IndexSpec, &Segment, &IndexSpec, &Segment) -> R,
) -> Result<R, &'static str> {
    SHARD_INDEXES.with(|tl| {
        let mut st = tl.borrow_mut();
        refresh(&mut st, store);
        let ia = st.idx.iter().position(|si| si.spec.name == a).ok_or("ERR no such index")?;
        let ib = st.idx.iter().position(|si| si.spec.name == b).ok_or("ERR no such index")?;
        for i in [ia, ib] {
            if matches!(st.idx[i].build, BuildState::Backfilling { .. }) {
                return Err("INDEXBUILDING index is still building");
            }
        }
        let (sa, sb) = (&st.idx[ia], &st.idx[ib]);
        Ok(f(&sa.spec, &sa.seg, &sb.spec, &sb.seg))
    })
}

/// Whether this shard's slice of `name` is still backfilling.
pub(crate) fn segment_building(store: &mut Store, name: &[u8]) -> bool {
    SHARD_INDEXES.with(|tl| {
        let mut st = tl.borrow_mut();
        refresh(&mut st, store);
        st.idx
            .iter()
            .find(|si| si.spec.name == name)
            .is_some_and(|si| matches!(si.build, BuildState::Backfilling { .. }))
    })
}

/// Reconcile the thread-local segment list with the global catalog:
/// keep segments whose spec is unchanged, start backfills for new
/// ones, drop removed ones.
fn refresh(st: &mut ShardIndexes, store: &mut Store) {
    let generation = CATALOG_GEN.load(Ordering::Acquire);
    if st.generation == generation {
        return;
    }
    let cat = catalog();
    let mut next: Vec<ShardIndex> = Vec::new();
    if let Some(cat) = cat {
        for (spec, _state) in cat.iter() {
            match st.idx.iter().position(|si| si.spec == *spec) {
                Some(i) => next.push(st.idx.swap_remove(i)),
                None => {
                    // Snapshot the domain's keys on THIS shard; live
                    // writes from now on hit the hook first and win.
                    let mut pat = spec.prefix.clone();
                    pat.push(b'*');
                    let keys = store.collect_keys(Some(&pat), None);
                    next.push(ShardIndex {
                        spec: spec.clone(),
                        seg: Segment::new(),
                        build: BuildState::Backfilling { keys, pos: 0 },
                    });
                }
            }
        }
    }
    st.idx = next;
    st.generation = generation;
}

/// Index one row: read the field from the hash at `key`, coerce,
/// apply. A missing key / non-hash / missing field clears the row.
fn apply_row(store: &mut Store, si: &mut ShardIndex, key: &[u8]) {
    let val = row_value(store, &si.spec, key);
    match val {
        RowValue::Value(v) => si.seg.apply(key, Some(v)),
        RowValue::CoerceFailed => si.seg.apply(key, None),
        RowValue::Gone => si.seg.remove(key),
    }
}

enum RowValue {
    Value(IndexValue),
    CoerceFailed,
    Gone,
}

fn row_value(store: &mut Store, spec: &IndexSpec, key: &[u8]) -> RowValue {
    match store.hget(key, &spec.field) {
        Ok(Some(raw)) => {
            let raw = raw.to_vec();
            match IndexValue::coerce(spec.ty, &raw) {
                Some(v) => RowValue::Value(v),
                None => RowValue::CoerceFailed,
            }
        }
        // `hget` answers None for BOTH a missing key and a missing
        // field; only the latter is a row excluded by coercion — a
        // missing key is simply not a row.
        Ok(None) => {
            if store.exists(&[key.to_vec()]) == 0 {
                RowValue::Gone
            } else {
                RowValue::CoerceFailed
            }
        }
        Err(_) => RowValue::Gone, // not a hash → not a row
    }
}

fn advance_backfill(store: &mut Store, si: &mut ShardIndex, batch: usize) {
    let BuildState::Backfilling { keys, pos } = &mut si.build else {
        return;
    };
    let end = (*pos + batch).min(keys.len());
    // Split the borrow: take the key slice out while applying.
    let slice: Vec<Vec<u8>> = keys[*pos..end].to_vec();
    *pos = end;
    let done = *pos >= keys.len();
    for key in &slice {
        // Hook-applied entries win: only fill keys not yet indexed.
        if si.seg.verify_entry(key).is_none() {
            apply_row_backfill(store, si, key);
        }
    }
    if done {
        si.build = BuildState::Ready;
    }
}

fn apply_row_backfill(store: &mut Store, si: &mut ShardIndex, key: &[u8]) {
    match row_value(store, &si.spec, key) {
        RowValue::Value(v) => si.seg.apply(key, Some(v)),
        RowValue::CoerceFailed => si.seg.apply(key, None),
        RowValue::Gone => {} // deleted since snapshot — nothing to do
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kevy_index::{IndexKind, ValType};

    fn spec(name: &str) -> IndexSpec {
        IndexSpec {
            name: name.into(),
            prefix: b"user:".to_vec(),
            field: b"age".to_vec(),
            ty: ValType::I64,
            kind: IndexKind::Range,
            max_bytes: 0,
        }
    }

    fn install_one(name: &str) {
        let mut c = Catalog::new();
        c.create(spec(name)).unwrap();
        install_catalog(c);
    }

    #[test]
    fn hook_backfill_and_query_lifecycle() {
        let mut store = Store::new();
        // Pre-existing rows (to be backfilled).
        store.hset(b"user:1", &[(b"age".to_vec(), b"30".to_vec())]).unwrap();
        store.hset(b"user:2", &[(b"age".to_vec(), b"25".to_vec())]).unwrap();
        store.hset(b"user:bad", &[(b"age".to_vec(), b"x".to_vec())]).unwrap();
        install_one("t_age");

        // Live write during Building: hook double-writes.
        on_write(&mut store, b"user:3");
        assert!(segment_building(&mut store, b"t_age"));
        assert!(with_ready_segment(&mut store, b"t_age", |_, _| ()).is_err());

        // user:3 has no hash yet — create it and write again (HSET path).
        store.hset(b"user:3", &[(b"age".to_vec(), b"40".to_vec())]).unwrap();
        on_write(&mut store, b"user:3");

        // Tick drains the backfill.
        on_tick(&mut store);
        let (hits, stats) = with_ready_segment(&mut store, b"t_age", |spec, seg| {
            let min = IndexValue::parse_literal(spec.ty, b"0").unwrap();
            let max = IndexValue::parse_literal(spec.ty, b"100").unwrap();
            (seg.range(&min, &max, None, 10).0, seg.stats())
        })
        .unwrap();
        assert_eq!(hits.len(), 3, "2 backfilled + 1 live");
        assert_eq!(hits[0].0, b"user:2".to_vec());
        assert_eq!(stats.coerce_failures, 1, "user:bad excluded");

        // Update moves the row; delete removes it.
        store.hset(b"user:1", &[(b"age".to_vec(), b"99".to_vec())]).unwrap();
        on_write(&mut store, b"user:1");
        store.del(&[b"user:2".to_vec()]);
        on_write(&mut store, b"user:2");
        let hits = with_ready_segment(&mut store, b"t_age", |spec, seg| {
            let min = IndexValue::parse_literal(spec.ty, b"0").unwrap();
            let max = IndexValue::parse_literal(spec.ty, b"100").unwrap();
            seg.range(&min, &max, None, 10).0
        })
        .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits.last().unwrap().0, b"user:1".to_vec());
        assert_eq!(hits.last().unwrap().1, IndexValue::I64(99));

        install_catalog(Catalog::new()); // cleanup for other tests
    }
}

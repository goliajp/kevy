//! The COMPOSE set algebra over two READY segments: AND keeps the A-hits
//! whose row B also holds in range; OR unions both ranges. Key-sorted,
//! cursor-trimmed, truncated to the limit.
//!
//! `LIMIT` does NOT bound the work here, and cannot: a segment is ordered
//! by VALUE while COMPOSE's result and its cursor are ordered by KEY, so
//! one key-ordered page needs the whole match set. The command reference
//! states this cost model.
//!
//! AND asks, for each A-hit, whether B holds the row in B's range. A
//! segment with a key directory answers that per key. Otherwise the two
//! ways to answer are walking B's range once into a set (its size is known
//! from the tree's counts before walking) or reading B's field from each
//! A-hit's row; the cheaper by count is taken.

use kevy_index::{IndexSpec, IndexValue, Segment};
use kevy_store::Store;

use super::args::ComposeQuery;
use crate::index_runtime;
use crate::state::Ctx;

/// A B-range this many times larger than the A-hits is checked row by row
/// instead of walked.
const WALK_B_UP_TO: usize = 4;

/// The composed key page, or `None` for bounds that do not parse.
pub(super) fn keys(
    ctx: &Ctx<'_>,
    store: &mut Store,
    cq: &ComposeQuery,
) -> Result<Option<Vec<Vec<u8>>>, kevy_resp::CmdError> {
    let found =
        index_runtime::with_two_ready_segments(ctx, &cq.a.name, &cq.b.name, |sa, a, sb, b| {
            find(cq, store, sa, a, sb, b)
        })?;
    let Some(mut keys) = found else { return Ok(None) };
    keys.sort();
    if let Some(cur) = &cq.cursor_key {
        keys.retain(|k| k.as_slice() > cur.as_slice());
    }
    keys.truncate(cq.limit);
    Ok(Some(keys))
}

fn find(
    cq: &ComposeQuery,
    store: &mut Store,
    sa: &IndexSpec,
    a: &Segment,
    sb: &IndexSpec,
    b: &Segment,
) -> Option<Vec<Vec<u8>>> {
    let (min_a, max_a) = super::ops::sub_bounds(&cq.a.shape, sa.ty())?;
    let (min_b, max_b) = super::ops::sub_bounds(&cq.b.shape, sb.ty())?;
    let (a_hits, _) = a.range(&min_a, &max_a, None, usize::MAX);
    let a_keys = a_hits.into_iter().map(|(k, _)| k);
    if !cq.and {
        let (b_hits, _) = b.range(&min_b, &max_b, None, usize::MAX);
        let mut all: Vec<Vec<u8>> = a_keys.chain(b_hits.into_iter().map(|(k, _)| k)).collect();
        all.sort();
        all.dedup();
        return Some(all);
    }
    let in_range = |v: &IndexValue| *v >= min_b && *v <= max_b;
    if let Some(dir) = b.key_dir() {
        return Some(a_keys.filter(|k| dir.get(k).as_ref().is_some_and(in_range)).collect());
    }
    let a_keys: Vec<Vec<u8>> = a_keys.collect();
    if b.count(&min_b, &max_b) as usize > WALK_B_UP_TO * a_keys.len().max(1) {
        // B's field from each row, then whether B holds the row under it
        // (a row outside a windowed B's hot range is not held)
        let held = |k: &Vec<u8>| {
            row_value(store, sb, k).is_some_and(|v| in_range(&v) && b.contains(&v, k))
        };
        return Some(a_keys.into_iter().filter(held).collect());
    }
    let (b_hits, _) = b.range(&min_b, &max_b, None, usize::MAX);
    let in_b: std::collections::HashSet<Vec<u8>> = b_hits.into_iter().map(|(k, _)| k).collect();
    Some(a_keys.into_iter().filter(|k| in_b.contains(k)).collect())
}

/// The value B's spec derives from the row at `key` now, read without
/// promoting a cold row.
fn row_value(store: &mut Store, spec: &IndexSpec, key: &[u8]) -> Option<IndexValue> {
    let names = spec.scalar_read_names();
    let vals = store.peek_hash_fields(key, &names[..spec.primary_width()]).ok()??;
    spec.derive_scalar(&vals)
}

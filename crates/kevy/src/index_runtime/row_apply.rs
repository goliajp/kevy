//! Row application — how one keyspace row becomes (or leaves) an
//! index entry, for every index kind, plus the tick-incremental
//! backfill driver. Split from the runtime parent for the 500-LOC
//! house rule.
//!
//! T6 (capacity arc): every row read here goes through the no-promote
//! peek — ONE record read + one decode per cold row covers the primary
//! field, the group field, and every declared VALUES column; nothing
//! promotes and the 2nd-touch gate never advances, so an IDX.CREATE
//! backfill (2048 rows/tick) or a live hook re-derive can never thrash
//! the hot tier (RFC 2026-07-24 §4 row 9).

use kevy_index::{IndexSpec, IndexValue, Segment};
use kevy_store::Store;

use super::{BuildState, ShardIndex};

/// Apply one scalar row: the coerced primary field, and — when the spec
/// declares `VALUES` — the row's stored values riding the same write.
///
/// T6: the primary field AND every declared VALUES column read in ONE
/// [`Store::peek_hash_fields`] row peek — a cold row costs one record
/// read + one decode (never one per field), promotes nothing and never
/// advances the 2nd-touch gate (backfill at 2048 rows/tick would
/// otherwise thrash the hot tier). The peek's `Ok(None)`/`Err` arms
/// replace the old `exists()` disambiguation probe exactly: a missing
/// key / non-hash is not a row; a present hash whose primary field is
/// missing or fails coercion is an excluded row.
pub(super) fn apply_scalar_row(store: &mut Store, spec: &IndexSpec, seg: &mut Segment, key: &[u8]) {
    // The driving columns (the composite's declared columns, or the
    // single FIELD) and the stored VALUES columns, one row peek. The
    // derivation itself — coercion or the composite byte encoding —
    // lives with the spec ([`IndexSpec::derive_scalar`]), so the
    // server and the embedded store cannot index one row differently.
    let names = spec.scalar_read_names();
    let w = spec.primary_width();
    match store.peek_hash_fields(key, &names) {
        Ok(None) | Err(_) => seg.remove(key),
        Ok(Some(vals)) => {
            let primary = spec.derive_scalar(&vals[..w]);
            match primary {
                None => seg.apply_with_values(key, None, &[]),
                Some(v) if spec.values.is_empty() => seg.apply(key, Some(v)),
                Some(v) => {
                    let refs: Vec<Option<&[u8]>> =
                        vals[w..].iter().map(|o| o.as_deref()).collect();
                    seg.apply_with_values(key, Some(v), &refs);
                }
            }
        }
    }
}

/// Index one row: read the field from the hash at `key`, coerce,
/// apply. A missing key / non-hash / missing field clears the row.
pub(super) fn apply_row(store: &mut Store, si: &mut ShardIndex, key: &[u8]) {
    // Agg kind: both fields must resolve — the aggregated value
    // coerces per the declared type, the group key is raw bytes.
    if let Some(a) = &mut si.agg {
        apply_row_agg(store, &si.spec, a, key);
        return;
    }
    // Ann kind: field bytes parse as an f32 vector (wrong shape
    // = excluded, same discipline as scalar coerce failure). T6: the
    // row peek — one record read on cold, no promotion, no gate mark.
    if let Some(g) = &mut si.ann {
        let v = match store.peek_hash_fields(key, &[si.spec.field()]) {
            Ok(Some(mut vals)) => {
                vals[0].take().and_then(|raw| kevy_vector::parse_vector(&raw, g.dim()))
            }
            _ => None,
        };
        g.apply(key, v);
        return;
    }
    // Text kind: raw field bytes tokenize into the inverted
    // segment (no scalar coercion).
    if let Some(ts) = &mut si.text {
        apply_row_text(store, &si.spec, ts, key);
        return;
    }
    apply_scalar_row(store, &si.spec, &mut si.seg, key);
}

/// [`apply_row`]'s text half. What the spec reads out of the row —
/// every declared field with its weight (they score into one corpus,
/// which is the whole reason multi-field is a spec change rather than
/// several single-field indexes) and every declared stored value. The
/// spec owns that mapping, so the server and the embedded store cannot
/// read a row differently. T6: every declared field + value is
/// prefetched with ONE row peek (one record read on a cold row, no
/// promotion, no gate mark); `read_row` resolves from the prefetch.
fn apply_row_text(
    store: &mut Store,
    spec: &IndexSpec,
    ts: &mut kevy_text::TextSegment,
    key: &[u8],
) {
    let names: Vec<&[u8]> = spec
        .fields
        .iter()
        .map(|f| f.name.as_slice())
        .chain(spec.values.iter().map(|v| v.name.as_slice()))
        .collect();
    let fetched = store.peek_hash_fields(key, &names).ok().flatten();
    let (fields, values) = spec.read_row(|f| {
        let vals = fetched.as_ref()?;
        names.iter().position(|n| *n == f).and_then(|i| vals[i].clone())
    });
    let vals: Vec<Option<&[u8]>> = values.iter().map(|v| v.as_deref()).collect();
    if fields.is_empty() {
        ts.apply_doc(key, None, &vals);
    } else {
        ts.apply_doc(key, Some(&fields), &vals);
    }
}

/// [`apply_row`]'s agg half: both fields must resolve — the aggregated
/// value coerces per the declared type, the group key is raw bytes.
fn apply_row_agg(
    store: &mut Store,
    spec: &IndexSpec,
    a: &mut kevy_index::AggSegment,
    key: &[u8],
) {
    // T6: both fields read with ONE row peek — a cold row costs one
    // record read, promotes nothing, never marks the gate. The peek's
    // `Ok(None)`/`Err` arms carry the deleted-vs-excluded distinction
    // the old `exists()` probe answered: missing key = not a row
    // (retract); a present hash missing/failing a field = excluded,
    // counted; a present non-hash = excluded, counted.
    let group_field = spec.group_by.as_deref().unwrap_or_default();
    match store.peek_hash_fields(key, &[group_field, spec.field()]) {
        Ok(Some(mut vals)) => {
            let group = vals[0].take();
            let val =
                vals[1].take().and_then(|raw| kevy_index::IndexValue::coerce(spec.ty, &raw));
            match (group, val) {
                (Some(g), Some(v)) => a.apply(key, Some((g, v)), false),
                _ => a.apply(key, None, true),
            }
        }
        Ok(None) => a.apply(key, None, false),
        Err(_) => a.apply(key, None, true),
    }
}

pub(crate) enum RowValue {
    Value(IndexValue),
    CoerceFailed,
    Gone,
}

pub(crate) fn row_value(store: &mut Store, spec: &IndexSpec, key: &[u8]) -> RowValue {
    // Composite (ORDERPATH) indexes: VERIFY recomputes the whole byte
    // derivation from the declared columns — one row peek, drift stays
    // falsifiable for the mechanical encoding too.
    if spec.composite.is_some() {
        let names = spec.scalar_read_names();
        return match store.peek_hash_fields(key, &names[..spec.primary_width()]) {
            Ok(None) | Err(_) => RowValue::Gone,
            Ok(Some(vals)) => match spec.derive_scalar(&vals) {
                Some(v) => RowValue::Value(v),
                None => RowValue::CoerceFailed,
            },
        };
    }
    match store.hget(key, spec.field()) {
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
            if store.exists(&[key]) == 0 {
                RowValue::Gone
            } else {
                RowValue::CoerceFailed
            }
        }
        Err(_) => RowValue::Gone, // not a hash → not a row
    }
}

pub(super) fn advance_backfill(store: &mut Store, si: &mut ShardIndex, batch: usize) {
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
        let already = match (&si.text, &si.ann, &si.agg) {
            (Some(ts), _, _) => ts.contains(key),
            (_, Some(g), _) => g.contains(key),
            (_, _, Some(a)) => a.contains(key),
            _ => si.seg.verify_entry(key).is_some(),
        };
        if !already {
            apply_row_backfill(store, si, key);
        }
    }
    // A MAXMEM budget is enforced at build time — declarative failure
    // instead of OOM. The tiering floor joins it (T5): a build whose
    // growing segment leaves the tier no demotable headroom fails the
    // same declarative way (the per-tick `reserved_bytes` feed already
    // counts this segment's current size).
    if (si.spec.max_bytes > 0 && si.seg.stats().approx_bytes > si.spec.max_bytes)
        || store.tier_index_floor_blocked(0)
    {
        si.seg = Segment::new();
        si.build = BuildState::FailedOverBudget;
        return;
    }
    if done {
        si.build = BuildState::Ready;
    }
}

fn apply_row_backfill(store: &mut Store, si: &mut ShardIndex, key: &[u8]) {
    if si.text.is_some() || si.ann.is_some() || si.agg.is_some() {
        apply_row(store, si, key);
        return;
    }
    // A key deleted since the snapshot resolves to `Gone` → `remove`,
    // which is a no-op on a segment that never held it.
    apply_scalar_row(store, &si.spec, &mut si.seg, key);
}

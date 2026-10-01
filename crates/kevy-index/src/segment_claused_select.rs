//! The per-candidate halves of the clause engine: FILTER, facet counts,
//! selection with DISTINCT, and the FILTER-path resume cursor.

use std::collections::HashMap;

use super::{FacetCounts, ScalarClauses, ScalarHit};
use crate::catalog::ValType;
use crate::seg_walk::Walker;
use crate::segment::Cursor;
use crate::value::{ValueTest, order_key};
use kevy_text::sorted_order;

/// Whether the walk's entry satisfies every predicate (ANDed). A row with
/// no value for a filtered field never passes; a segment that stores no
/// values at all fails every filtered candidate — absent is not a value,
/// in either shape.
pub(super) fn passes(w: &Walker<'_>, filters: &[(usize, ValueTest)], buf: &mut Vec<u8>) -> bool {
    filters.iter().all(|(f, t)| w.column(*f, buf).is_some_and(|raw| t.passes(raw)))
}

/// A stored value's coerced key for a clause: order key under the
/// declared type; `None` = no usable value (its own group / sorts last).
fn clause_key(w: &Walker<'_>, field: usize, ty: ValType, buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    w.column(field, buf).and_then(|raw| order_key(ty, raw))
}

/// Credit one passing candidate to every facet bucket it has a value in.
/// Buckets key by the coerced identity; the label is a spelling that
/// occurs in the corpus. Rows without a value (or with one that does not
/// coerce) are in no bucket.
pub(super) fn count_facets(
    w: &Walker<'_>,
    c: &ScalarClauses<'_>,
    facets: &mut [FacetCounts],
    buf: &mut Vec<u8>,
) {
    for ((f, ty), counts) in c.facets.iter().zip(facets.iter_mut()) {
        let Some(raw) = w.column(*f, buf) else { continue };
        let Some(id) = order_key(*ty, raw) else { continue };
        match counts.get_mut(&id) {
            Some(e) => e.1 += 1,
            None => {
                counts.insert(id, (raw.to_vec(), 1));
            }
        }
    }
}

/// Push one passing candidate onto the page, collapsing under `DISTINCT`
/// during selection: in driving order the first occurrence of a value is
/// its best; under `SORT` the better group representative by the page's
/// own order replaces the held one. Rows with no value are their own
/// group and never collapse.
pub(super) fn select_hit(
    w: &mut Walker<'_>,
    c: &ScalarClauses<'_>,
    hits: &mut Vec<ScalarHit>,
    groups: &mut HashMap<Vec<u8>, usize>,
    buf: &mut Vec<u8>,
) {
    let okey = c.sort.and_then(|(f, _, ty)| clause_key(w, f, ty, buf));
    let dkey = c.distinct.and_then(|(f, ty)| clause_key(w, f, ty, buf));
    if let Some(id) = &dkey {
        match groups.entry(id.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => {
                let Some((_, order, _)) = c.sort else { return };
                let k = w.key();
                let prev = &mut hits[*e.get()];
                if sorted_order((okey.as_deref(), k), (prev.okey.as_deref(), &prev.key), order)
                    == std::cmp::Ordering::Less
                {
                    let (v, k) = w.pair();
                    *prev = ScalarHit { key: k.to_vec(), value: v.clone(), okey, dkey };
                }
                return;
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(hits.len());
            }
        }
    }
    let (v, k) = w.pair();
    hits.push(ScalarHit { key: k.to_vec(), value: v.clone(), okey, dkey });
}

/// The resume cursor for the FILTER-with-CURSOR path: the last served
/// `(value, key)`, exactly as the plain range emits it. Selection clauses
/// page nothing, so they carry none.
pub(super) fn filter_cursor(c: &ScalarClauses<'_>, hits: &[ScalarHit]) -> Option<Cursor> {
    if c.selects() || hits.len() < c.fetch {
        return None;
    }
    hits.last().map(|h| Cursor { value: h.value.clone(), key: h.key.clone() })
}

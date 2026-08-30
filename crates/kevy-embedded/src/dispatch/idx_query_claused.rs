//! The clause-carrying scalar IDX.QUERY reply — the embedded twin of
//! the server's `query_claused` op + claused reduce, byte-identical on
//! the wire (the dispatch oracle pins it). Split from `idx_query.rs`
//! for the 500-LOC house rule (a `#[path]` child).

use crate::store::Store;

use kevy_index::IndexValue;

use super::super::idx::{encode_cursor, value_repr};
use super::super::util::{arr, bulk, err};
use super::{emit_row, idx_err, tail};

/// The clause-carrying scalar page, matching the server reduce's reply
/// bytes: the `[cursor, rows]` envelope, rows in the merged order, and
/// a `FACET` query's rows array gaining ONE trailing element.
pub(super) fn claused_query(
    s: &Store,
    name: &[u8],
    min: &IndexValue,
    max: &IndexValue,
    cursor: Option<&kevy_index::Cursor>,
    t: &tail::Tail,
    out: &mut Vec<u8>,
) {
    let filters: Vec<crate::ValueFilter> =
        t.filters.iter().map(tail::FilterClause::as_value_filter).collect();
    let opts = crate::ScalarQueryOpts {
        filters: &filters,
        sort: t.sort.as_ref().map(|(f, d)| (f.as_slice(), *d)),
        distinct: t.distinct.as_deref(),
        facets: &t.facets,
        offset: t.offset,
    };
    match s.idx_query_claused(name, min, max, cursor, t.limit, opts) {
        Err(crate::KevyError::InvalidInput(m)) => {
            // A clause this index cannot answer — the server frames it
            // as `ERR <verb> '<name>': <explanation>`.
            let n = String::from_utf8_lossy(name);
            err(out, &format!("ERR IDX.QUERY '{n}': {m}"));
        }
        Err(e) => idx_err(out, name, &e),
        Ok(page) => {
            arr(out, 2);
            match &page.cursor {
                Some(c) => bulk(out, &encode_cursor(&c.value, &c.key)),
                None => bulk(out, b"0"),
            }
            let extra = usize::from(!t.facets.is_empty());
            if t.fields.is_empty() {
                arr(out, page.rows.len() * 2 + extra);
                for (k, v) in &page.rows {
                    bulk(out, k);
                    bulk(out, &value_repr(v));
                }
            } else {
                arr(out, page.rows.len() + extra);
                for (k, v) in &page.rows {
                    emit_row(s, out, k, Some(v), &t.fields);
                }
            }
            emit_scalar_facets(out, &t.facets, &page.facets);
        }
    }
}

/// The trailing facet element: `[field, [label, count, …], field, …]`.
fn emit_scalar_facets(out: &mut Vec<u8>, names: &[Vec<u8>], buckets: &[Vec<(Vec<u8>, u64)>]) {
    if names.is_empty() {
        return;
    }
    arr(out, names.len() * 2);
    for (name, field) in names.iter().zip(buckets) {
        bulk(out, name);
        arr(out, field.len() * 2);
        for (label, n) in field {
            bulk(out, label);
            bulk(out, n.to_string().as_bytes());
        }
    }
}

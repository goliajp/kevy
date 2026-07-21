//! Turning a MATCH's clauses into what the segment takes: `IN` names
//! into field positions, `FILTER` predicates into tests. Split from
//! `ops.rs` for the 500-LOC house rule (a `#[path]` child, so it shares
//! the op module's imports of the wire statuses).
//!
//! Both need the index spec, which only the shard holding the segment
//! has — which is also why the errors are built here, carrying the whole
//! explanation rather than a status the origin would have to guess at.

/// Map an `IN <field…>` clause's names onto the segment's field
/// positions, in declaration order.
///
/// `Err` carries a ready-made error chunk naming the field that is not
/// declared, and listing the ones that are. Scoping to an undeclared
/// field could just match nothing — but a typo in a field name would
/// then return an empty result that looks exactly like a working query
/// over a corpus with no hits, which is the one failure mode a search
/// engine must not have.
pub(super) fn scope_positions(
    spec: &kevy_index::IndexSpec,
    scope: &[Vec<u8>],
) -> Result<Vec<usize>, Vec<u8>> {
    let mut out = Vec::with_capacity(scope.len());
    for want in scope {
        match spec.fields.iter().position(|f| f.name == *want) {
            Some(i) => out.push(i),
            None => {
                let declared: Vec<&[u8]> =
                    spec.fields.iter().map(|f| f.name.as_slice()).collect();
                return Err(clause_error("IN", want, "index", &declared));
            }
        }
    }
    Ok(out)
}

/// A per-shard chunk explaining a clause this index cannot answer,
/// naming what it DOES offer. Only the shard holding the spec knows
/// that, which is why the explanation is built here and carried whole.
fn clause_error(clause: &str, bad: &[u8], verb: &str, offered: &[&[u8]]) -> Vec<u8> {
    let mut chunk = vec![crate::cmd_index_query::ST_CLAUSE];
    chunk.extend_from_slice(
        format!(
            "{clause} names field '{}', which this index does not {verb} — it {verb}es: {}",
            String::from_utf8_lossy(bad),
            String::from_utf8_lossy(&offered.join(&b", "[..])),
        )
        .as_bytes(),
    );
    chunk
}


/// [`filter_preds`] with each test boxed as the closure the segment
/// takes. The boxes must outlive the borrowed `Filter` list, so they are
/// returned rather than built inline.
type ValuePred = Box<dyn Fn(&[u8]) -> bool>;
type BoxedPred = (usize, ValuePred);

pub(super) fn boxed_preds(
    spec: &kevy_index::IndexSpec,
    filters: &[super::super::args::FilterArg],
) -> Result<Vec<BoxedPred>, Vec<u8>> {
    Ok(filter_preds(spec, filters)?
        .into_iter()
        .map(|(field, t)| {
            let f: ValuePred = Box::new(move |v: &[u8]| t.passes(v));
            (field, f)
        })
        .collect())
}

/// Build the segment-level predicates for a `FILTER` clause: map each
/// named field onto its stored-value position and build the comparison
/// with the type that field was DECLARED as.
///
/// The comparison itself lives with the spec (`ValueTest`), so the server
/// and the embedded store cannot disagree about what a bound means.
fn filter_preds(
    spec: &kevy_index::IndexSpec,
    filters: &[super::super::args::FilterArg],
) -> Result<Vec<(usize, kevy_index::ValueTest)>, Vec<u8>> {
    use super::super::args::FilterShape;
    let mut out = Vec::with_capacity(filters.len());
    for f in filters {
        let Some(pos) = spec.values.iter().position(|v| v.name == f.field) else {
            let stored: Vec<&[u8]> = spec.values.iter().map(|v| v.name.as_slice()).collect();
            return Err(clause_error("FILTER", &f.field, "store", &stored));
        };
        let ty = spec.values[pos].ty;
        let (test, raw) = match &f.shape {
            FilterShape::Range { min, max } => (kevy_index::ValueTest::range(ty, min, max), min),
            FilterShape::Eq { value } => (kevy_index::ValueTest::eq(ty, value), value),
        };
        let Some(test) = test else {
            let mut chunk = vec![crate::cmd_index_query::ST_CLAUSE];
            chunk.extend_from_slice(
                format!(
                    "FILTER bound '{}' is not a valid {}, which is how this index declares '{}'",
                    String::from_utf8_lossy(raw),
                    ty.tag(),
                    String::from_utf8_lossy(&f.field),
                )
                .as_bytes(),
            );
            return Err(chunk);
        };
        out.push((pos, test));
    }
    Ok(out)
}


/// Resolve a `SORT <field> ASC|DESC` clause: which stored value orders
/// the page, and the order-preserving encoding of that field's type.
///
/// Errors when the field is not stored, naming what is — the same
/// contract `IN` and `FILTER` keep.
pub(super) fn sort_field(
    spec: &kevy_index::IndexSpec,
    sort: &Option<(Vec<u8>, bool)>,
) -> Result<Option<(usize, bool, kevy_index::ValType)>, Vec<u8>> {
    let Some((field, desc)) = sort else { return Ok(None) };
    let Some(pos) = spec.values.iter().position(|v| v.name == *field) else {
        let stored: Vec<&[u8]> = spec.values.iter().map(|v| v.name.as_slice()).collect();
        return Err(clause_error("SORT", field, "store", &stored));
    };
    Ok(Some((pos, *desc, spec.values[pos].ty)))
}

//! Fresh per-kind segments shaped by an index's spec — shared by the
//! catalog refresh, the FLUSH reset and a failed build.

use kevy_index::{IndexSpec, Segment};

/// A fresh scalar segment for `spec` — with the stored-value
/// side-channel iff a scalar kind declared `VALUES` (text keeps its
/// values in the text segment; without the declaration this is the
/// plain `Segment::new()`, byte-identical to before — A5).
pub(super) fn new_scalar_seg(spec: &IndexSpec) -> Segment {
    Segment::for_spec(spec)
}

/// A fresh text segment for `spec` when it is a text index — with the
/// positional side-channel iff it was created WITH POSITIONS.
/// A fresh HNSW graph shaped by the spec (None for non-ann kinds) —
/// shared by the catalog refresh and the FLUSH reset.
pub(super) fn new_ann_seg(spec: &kevy_index::IndexSpec) -> Option<kevy_vector::Hnsw> {
    spec.ann().as_ref().map(|a| {
        kevy_vector::Hnsw::new(
            a.dim as usize,
            kevy_vector::HnswParams::default()
                .with_m(a.m as usize)
                .with_ef_construction(a.ef as usize)
                .with_distance(match a.distance {
                    1 => kevy_vector::Distance::L2,
                    2 => kevy_vector::Distance::Ip,
                    _ => kevy_vector::Distance::Cosine,
                }),
        )
    })
}

pub(super) fn new_text_seg(spec: &kevy_index::IndexSpec) -> Option<kevy_text::TextSegment> {
    (spec.kind() == kevy_index::IndexKind::Text).then(|| {
        // The declared field count decides whether the segment keeps the
        // per-field breakdown `IN <field…>` scopes to; one field needs
        // none, because its per-field numbers are the merged ones.
        kevy_text::TextSegment::with_shape(
            kevy_text::SegmentShape::default()
                .with_fields(spec.fields().len())
                .with_positions(spec.has_positions())
                .with_values(spec.values().len()),
        )
    })
}

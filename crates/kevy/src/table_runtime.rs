//! The table catalog's own write hook.
//!
//! Split from the index hook because the two conditions differ: a table that
//! declares columns and no index is legal, and its rows are as declared as
//! any other. Hanging the representation off `IDX_NONEMPTY` made a
//! convenience of implementation into a precondition of the feature — the
//! rows silently kept the general form, and the end-to-end measurement read
//! as "the packed row barely helps" when nothing had been packed at all.

use kevy_store::Store;

use crate::state::Ctx;

/// Give a row under a declared prefix the packed representation.
///
/// The store cannot decide this: which table a key belongs to is the
/// catalog's answer and the catalog lives here. This is the same prefix walk
/// the index hook above already does, so a key outside every declared table
/// costs one comparison per table and nothing else.
///
/// Off unless `Store::set_packed_rows` is on. The conversion reads the row
/// back and builds a buffer, so it is paid once per row — at its FIRST write,
/// which for a bulk load is every row. That is where the cost shows, and it
/// is measured on the load axis rather than argued about.
pub(crate) fn on_write(ctx: &Ctx<'_>, store: &mut Store, key: &[u8]) {
    if !store.packed_rows_enabled() {
        return;
    }
    let Some(tables) = ctx.state.catalogs.table() else { return };
    let Some(spec) = tables.iter().find(|t| key.starts_with(&t.prefix)) else { return };
    let names: Vec<Vec<u8>> = spec.columns.iter().map(|(n, _)| n.clone()).collect();
    store.pack_row(key, &names);
}

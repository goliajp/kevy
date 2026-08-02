//! The window's per-tick orchestration on one shard: the eviction +
//! slide step of a windowed scalar index, the text-freeze fan-out
//! over the same batch, and the small name/catalog lookups they and
//! [`super::refresh`] share. Split from `index_runtime.rs` by
//! responsibility — this file is what MOVES data at the window edge;
//! the parent owns lifecycle and access.

use kevy_index::IndexSpec;
use kevy_store::Store;

use super::{BuildState, Segment, ShardIndexes};
use crate::state::CatalogState;

/// Freeze one eviction batch out of every text index of `table`.
/// Failure leaves the entries hot — derived spill, semantics intact,
/// logged and retried never (the batch's window has already slid).
pub(super) fn freeze_text_batches(st: &mut ShardIndexes, table: &[u8], keys: &[Vec<u8>], dir: &std::path::Path) {
    for si in &mut st.idx {
        let (Some(cold), Some(ts), BuildState::Ready) =
            (&mut si.cold_text, &mut si.text, &si.build)
        else {
            continue;
        };
        if table_of(&si.spec.name) != table {
            continue;
        }
        match cold.freeze_batch(ts, &si.spec.name, keys, dir) {
            Ok(true) => st.stats_dirty = true,
            Ok(false) => {}
            Err(e) => eprintln!(
                "kevy: text freeze '{}': {e}",
                String::from_utf8_lossy(&si.spec.name)
            ),
        }
    }
}

/// One windowed index's eviction step — rows first, index second (a
/// failed row eviction skips the cut so the whole batch retries next
/// tick), with a post-slide malloc_trim: a slide bulk-frees a whole
/// bucket's values and glibc keeps the arena unless told. Returns
/// whether the index tree changed. `drives_rows` is the per-table
/// row-eviction mandate ([`kevy_index::window_driver`]): a non-driver
/// windowed path slides its own tree and touches no rows.
pub(super) fn evict_and_slide(
    win: &mut kevy_window::WindowRt,
    name: &[u8],
    seg: &mut Segment,
    store: &mut Store,
    dir: &std::path::Path,
    drives_rows: bool,
) -> bool {
    if drives_rows && let Some(rows) = win.pending_rows(seg) {
        let sealed = store
            .enable_seg_rows(dir)
            .and_then(|()| store.seal_rows_to_seg(table_of(name), &rows));
        match sealed {
            Ok(None) => {}
            Ok(Some(batch)) => {
                crate::kevy_rt_push_tick_frame(&batch.file);
                store.commit_row_eviction(&batch);
            }
            Err(e) => {
                eprintln!("kevy: row eviction '{}': {e}", String::from_utf8_lossy(name));
                return false;
            }
        }
    }
    match win.slide(name, seg, dir) {
        Ok(true) => {
            kevy_sys::malloc_trim_now();
            true
        }
        Ok(false) => false,
        Err(e) => {
            eprintln!("kevy: window slide '{}': {e}", String::from_utf8_lossy(name));
            false
        }
    }
}

/// The table half of a compiled index name (`<table>.<col>`).
pub(super) fn table_of(index_name: &[u8]) -> &[u8] {
    let dot = index_name.iter().position(|&b| b == b'.').unwrap_or(index_name.len());
    &index_name[..dot]
}

/// [`kevy_index::window_for`] against the shared catalog state.
pub(super) fn window_for(
    catalogs: &CatalogState,
    spec: &IndexSpec,
) -> Option<(kevy_index::WindowSpec, kevy_index::WindowShape)> {
    kevy_index::window_for(catalogs.table()?.as_ref(), &spec.name)
}

/// [`kevy_index::window_driver`] against the shared catalog state.
pub(super) fn window_driver(catalogs: &CatalogState, index_name: &[u8]) -> bool {
    catalogs.table().is_some_and(|t| kevy_index::window_driver(t.as_ref(), index_name))
}

/// Whether this compiled index is a windowed table's TEXT index —
/// its documents freeze into cold bucket segments as the window
/// slides. (The batch discovery lives on the window column's scalar
/// index; this index only needs a cold directory.)
pub(super) fn text_window_for(catalogs: &CatalogState, spec: &IndexSpec) -> bool {
    if spec.kind != kevy_index::IndexKind::Text {
        return false;
    }
    let Some(dot) = spec.name.iter().position(|&b| b == b'.') else { return false };
    let Some(cat) = catalogs.table() else { return false };
    cat.get(&spec.name[..dot]).is_some_and(|t| t.window.is_some())
}

/// The per-shard segment directory, when persistence is on. No data
/// dir = memory-only deployment = nothing slides (declaring a window
/// is allowed; it simply stays all-hot).
pub(super) fn shard_segs_dir(
    state: &crate::state::RuntimeState,
    shard_id: usize,
) -> Option<std::path::PathBuf> {
    state.sidecar_dir().map(|d| kevy_persist::layout::segs_dir(d, shard_id))
}


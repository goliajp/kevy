//! The embedded face's half of the sliding window: the reaper tick
//! reconciles per-index [`WindowRt`]s against the table catalog and
//! slides them; queries and the write path consult them through
//! [`ShardSegs::window_of`] / [`ShardSegs::window_mut`]. One shared
//! runtime (`kevy-window`) serves both engine faces, so the slide,
//! the tombstones and the cold merge cannot drift from the server's.

use std::path::Path;

use kevy_window::WindowRt;

use crate::ops_index::ShardSegs;
use crate::ops_table::TableReg;

/// One index's eviction step: rows first, index second — a failed row
/// eviction skips the index cut so the whole batch retries next tick.
/// Returns whether the index tree changed.
#[allow(clippy::too_many_arguments)]
fn evict_and_slide(
    win: &mut WindowRt,
    spec: &kevy_index::IndexSpec,
    seg: &mut kevy_index::Segment,
    store: &mut kevy_store::Store,
    aof: &mut Option<kevy_persist::Aof>,
    segs_dir: &Path,
    drives_rows: bool,
) -> bool {
    // Exactly ONE windowed access path per table drives row eviction
    // (kevy_index::window_driver); every other one slides only its
    // own tree.
    if drives_rows
        && let Some(rows) = win.pending_rows(seg)
        && !evict_rows(&spec.name, &rows, store, aof, segs_dir)
    {
        return false;
    }
    let moved = win.slide(&spec.name, seg, segs_dir).unwrap_or_else(|e| {
        eprintln!("kevy-embedded: window slide '{}': {e}", String::from_utf8_lossy(&spec.name));
        false
    });
    // Same bulk-free contract as the server tick: ask glibc to return
    // the slid bucket's arena to the OS.
    if moved {
        kevy_sys::malloc_trim_now();
    }
    moved
}

/// The row half of one eviction: seal, frame (R2c ordering — after
/// the durable copy, before the phase change; AOF only, replicas seal
/// their own segments), commit. `false` = retry whole next tick.
fn evict_rows(
    name: &[u8],
    rows: &[Vec<u8>],
    store: &mut kevy_store::Store,
    aof: &mut Option<kevy_persist::Aof>,
    segs_dir: &Path,
) -> bool {
    let sealed =
        store.enable_seg_rows(segs_dir).and_then(|()| store.seal_rows_to_seg(table_of(name), rows));
    match sealed {
        Ok(None) => true,
        Ok(Some(batch)) => {
            if let Some(a) = aof {
                let argv = kevy_persist::segmented_argv(batch.file.as_bytes());
                let owned: Vec<Vec<u8>> = argv.iter().map(|x| x.to_vec()).collect();
                if let Err(e) = a.append(&owned_argv(&owned)) {
                    eprintln!(
                        "kevy-embedded: SEGMENTED frame '{}': {e}",
                        String::from_utf8_lossy(name)
                    );
                    return false;
                }
            }
            store.commit_row_eviction(&batch);
            true
        }
        Err(e) => {
            eprintln!("kevy-embedded: row eviction '{}': {e}", String::from_utf8_lossy(name));
            false
        }
    }
}

/// An owned argv as the AOF's ArgvView.
fn owned_argv(parts: &[Vec<u8>]) -> kevy_persist::Argv {
    let total: usize = parts.iter().map(Vec::len).sum();
    let mut argv = kevy_persist::Argv::with_capacity(parts.len(), total);
    for p in parts {
        argv.push(p);
    }
    argv
}

/// The table half of a compiled index name (`<table>.<col>`).
fn table_of(index_name: &[u8]) -> &[u8] {
    let dot = index_name.iter().position(|&b| b == b'.').unwrap_or(index_name.len());
    &index_name[..dot]
}

/// One reaper tick's window work for one shard: reconcile the window
/// set against the table catalog (declare/replace/drop all converge),
/// then slide every windowed index whose boundary moved.
pub(crate) fn window_tick(
    segs: &mut ShardSegs,
    store: &mut kevy_store::Store,
    aof: &mut Option<kevy_persist::Aof>,
    tables: &TableReg,
    segs_dir: &Path,
) {
    let cat = tables.catalog.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut moved = false;
    // The driver's eviction batches, kept so the second pass can
    // freeze the SAME rows out of each table's text indexes (the
    // server tick's exact two-pass shape).
    #[cfg(feature = "text")]
    let mut batches: Vec<(Vec<u8>, Vec<Vec<u8>>)> = Vec::new();
    {
        // Disjoint borrows: the seg list walks, the window list edits.
        let seg_list = &mut segs.segs;
        let windows = &mut segs.windows;
        for (spec, seg) in seg_list.iter_mut() {
            reconcile_window(windows, &cat, &spec.name);
            let Some(win) = windows.iter_mut().find(|(n, _)| n == &spec.name).map(|(_, w)| w)
            else {
                continue;
            };
            let drives = kevy_index::window_driver(&cat, &spec.name);
            #[cfg(feature = "text")]
            if drives && let Some(rows) = win.pending_rows(seg) {
                batches.push((table_of(&spec.name).to_vec(), rows));
            }
            moved |= evict_and_slide(win, spec, seg, store, aof, segs_dir, drives);
        }
        // An index dropped from the catalog drops its window with it.
        let names: Vec<Vec<u8>> = seg_list.iter().map(|(s, _)| s.name.clone()).collect();
        windows.retain(|(n, _)| names.contains(n));
    }
    #[cfg(feature = "text")]
    {
        reconcile_cold_text(segs, &cat);
        moved |= freeze_text_batches(segs, &batches, segs_dir);
    }
    if moved {
        segs.mark_stats_dirty();
    }
}

/// Keep one index's window runtime in lockstep with the catalog:
/// declare/replace/drop (and a changed shape) all converge here.
fn reconcile_window(
    windows: &mut Vec<(Vec<u8>, WindowRt)>,
    cat: &kevy_index::TableCatalog,
    name: &[u8],
) {
    let want = kevy_index::window_for(cat, name);
    let at = windows.iter().position(|(n, _)| n == name);
    match (at, want) {
        (Some(i), None) => {
            windows.swap_remove(i);
        }
        (None, Some((w, sh))) => {
            windows.push((name.to_vec(), WindowRt::new(w, sh)));
        }
        (Some(i), Some((w, sh))) if windows[i].1.spec != w || windows[i].1.shape != sh => {
            windows[i].1 = WindowRt::new(w, sh);
        }
        _ => {}
    }
}

/// Keep the cold-text directory list in lockstep with the catalog: a
/// windowed table's text indexes each get one, everything else loses
/// its entry (declare/replace/drop all converge here).
#[cfg(feature = "text")]
fn reconcile_cold_text(segs: &mut ShardSegs, cat: &kevy_index::TableCatalog) {
    let want: Vec<(Vec<u8>, bool)> = segs
        .text
        .iter()
        .map(|(spec, _)| (spec.name.clone(), kevy_index::window_text_for(cat, spec)))
        .collect();
    for (name, wanted) in &want {
        let at = segs.cold_text.iter().position(|(n, _)| n == name);
        match (at, wanted) {
            (Some(i), false) => {
                segs.cold_text.swap_remove(i);
            }
            (None, true) => {
                segs.cold_text.push((name.clone(), kevy_window::TextColdDir::new()));
            }
            _ => {}
        }
    }
    segs.cold_text.retain(|(n, _)| want.iter().any(|(nn, _)| nn == n));
}

/// Freeze each eviction batch out of every text index of its table.
/// Failure leaves the entries hot — derived spill, semantics intact.
#[cfg(feature = "text")]
fn freeze_text_batches(
    segs: &mut ShardSegs,
    batches: &[(Vec<u8>, Vec<Vec<u8>>)],
    segs_dir: &Path,
) -> bool {
    let mut changed = false;
    for (table, keys) in batches {
        for (spec, ts) in segs.text.iter_mut() {
            if table_of(&spec.name) != table {
                continue;
            }
            let Some((_, dir)) = segs.cold_text.iter_mut().find(|(n, _)| n == &spec.name)
            else {
                continue;
            };
            match dir.freeze_batch(ts, &spec.name, keys, segs_dir) {
                Ok(true) => changed = true,
                Ok(false) => {}
                Err(e) => eprintln!(
                    "kevy-embedded: text freeze '{}': {e}",
                    String::from_utf8_lossy(&spec.name)
                ),
            }
        }
    }
    changed
}

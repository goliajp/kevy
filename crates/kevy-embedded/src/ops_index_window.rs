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

impl ShardSegs {
    /// The window runtime for `name`, if that index is a windowed
    /// table's window access path AND a tick has reconciled it.
    pub(crate) fn window_of(&self, name: &[u8]) -> Option<&WindowRt> {
        self.windows.iter().find(|(n, _)| n == name).map(|(_, w)| w)
    }
}

/// One reaper tick's window work for one shard: reconcile the window
/// set against the table catalog (declare/replace/drop all converge),
/// then slide every windowed index whose boundary moved.
pub(crate) fn window_tick(segs: &mut ShardSegs, tables: &TableReg, segs_dir: &Path) {
    let cat = tables.catalog.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut moved = false;
    {
        // Disjoint borrows: the seg list walks, the window list edits.
        let seg_list = &mut segs.segs;
        let windows = &mut segs.windows;
        for (spec, seg) in seg_list.iter_mut() {
            let want = kevy_index::window_for(&cat, &spec.name);
            let at = windows.iter().position(|(n, _)| n == &spec.name);
            match (at, want) {
                (Some(i), None) => {
                    windows.swap_remove(i);
                }
                (None, Some(w)) => windows.push((spec.name.clone(), WindowRt::new(w))),
                (Some(i), Some(w)) if windows[i].1.spec != w => {
                    windows[i].1 = WindowRt::new(w);
                }
                _ => {}
            }
            let Some(win) = windows.iter_mut().find(|(n, _)| n == &spec.name).map(|(_, w)| w)
            else {
                continue;
            };
            match win.slide(&spec.name, seg, segs_dir) {
                Ok(true) => moved = true,
                Ok(false) => {}
                Err(e) => eprintln!(
                    "kevy-embedded: window slide '{}': {e}",
                    String::from_utf8_lossy(&spec.name)
                ),
            }
        }
        // An index dropped from the catalog drops its window with it.
        let names: Vec<Vec<u8>> = seg_list.iter().map(|(s, _)| s.name.clone()).collect();
        windows.retain(|(n, _)| names.contains(n));
    }
    if moved {
        segs.mark_stats_dirty();
    }
}

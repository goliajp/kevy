//! The index, view and table catalog as recorded state.
//!
//! Every change is recorded as one internal frame carrying the whole
//! catalog, `XINTERNAL.CATALOG <lineage> <version> <index> <view> <table>`
//! (each catalog in its text encoding), to the log of the shard that made
//! it and to that shard's replicas. Every snapshot and every rewritten log
//! carries the current one beside the keyspace. A frame is applied only
//! when it is newer than what the node holds: `(lineage, version)` orders
//! them, so frames replayed from several shards' logs, in whatever order
//! the shards reach them, leave the newest. The lineage is minted by the
//! first change a node records or the first snapshot a primary takes,
//! carried by every frame, and adopted by whoever applies one, so a
//! promoted replica goes on from its primary's versions. A full sync
//! takes its snapshot's frame the same way, except that a frame from
//! another lineage (another primary) replaces the catalog even when it is
//! older.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use kevy_index::{Catalog, TableCatalog, ViewCatalog};
use kevy_resp::{Argv, ArgvView, encode_error, encode_simple_string};
use kevy_rt::propagation::{Propagate, set_override};
use kevy_store::Store;

use crate::state::{CatalogChange, Ctx, RuntimeState};

/// The files the catalog lived in before it was recorded state (6.4 and
/// earlier). Read once, when every shard has restored and neither log nor
/// snapshot held a catalog; removed once one does.
pub(crate) const SIDECARS: [&str; 3] =
    ["index-catalog.meta", "view-catalog.meta", "table-catalog.meta"];

#[derive(Debug, Default)]
pub(crate) struct RecordState {
    /// `(lineage, version)`; lineage 0 = nothing recorded yet. Taken before
    /// the catalogs' own hold when both are held.
    at: Mutex<(u64, u64)>,
    restored: AtomicUsize,
}

impl RecordState {
    fn lock(&self) -> MutexGuard<'_, (u64, u64)> {
        self.at.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The catalog as it now stands, recorded at the next version. The
/// caller holds `at` and then the catalogs, so the version moves and the
/// catalog is read with no change able to land in between: a higher
/// version never records less than a lower one, and replay, which keeps
/// the highest, keeps every change.
fn next_frame(state: &RuntimeState, at: &mut (u64, u64)) -> Vec<Vec<u8>> {
    mint(at);
    at.1 += 1;
    frame(state, *at)
}

/// Give a node that has recorded nothing its lineage.
fn mint(at: &mut (u64, u64)) {
    if at.0 == 0 {
        at.0 = kevy_store::now_unix_ms().max(1);
    }
}

/// Install `change` if the catalogs still carry `generation`, and record
/// the catalog it leaves as the frame of the command that made it (a
/// catalog command, or the reduce of a sampled declaration, a global
/// rebuild or an automatic declaration). `false` when another change
/// landed first. A change replayed from a log or a primary is recorded by
/// what carried it.
pub(crate) fn commit(state: &RuntimeState, generation: u64, change: CatalogChange) -> bool {
    let mut at = state.catalogs.record.lock();
    let _held = state.catalogs.hold();
    if state.catalogs.generation() != generation {
        return false;
    }
    state.install_catalogs(change);
    if !kevy_rt::applying_record() {
        set_override(Propagate::Replace(next_frame(state, &mut at)));
    }
    true
}

/// The current catalog as the frame that records it at `at`. The caller
/// holds the catalogs still.
fn frame(state: &RuntimeState, at: (u64, u64)) -> Vec<Vec<u8>> {
    let c = &state.catalogs;
    let index = c.index().map(|x| x.to_sidecar()).unwrap_or_default();
    let view = c.view().map(|x| x.to_sidecar()).unwrap_or_default();
    let table = c.table().map(|x| x.to_sidecar()).unwrap_or_default();
    vec![
        kevy_resp::ops_table::CATALOG.as_bytes().to_vec(),
        at.0.to_string().into_bytes(),
        at.1.to_string().into_bytes(),
        index.into_bytes(),
        view.into_bytes(),
        table.into_bytes(),
    ]
}

/// The catalog commands a client sends; `false` = not one of them. One
/// that changes nothing records nothing, not even its own argv: the
/// catalog is recorded only as the frame its commit leaves.
pub(crate) fn dispatch<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    if !matches!(
        cmd,
        b"IDX.CREATE"
            | b"IDX.DROP"
            | b"VIEW.CREATE"
            | b"VIEW.DROP"
            | b"TABLE.DECLARE"
            | b"TABLE.ENSURE"
            | b"TABLE.REPLACE"
            | b"TABLE.DROP"
    ) {
        return false;
    }
    if !kevy_rt::applying_record() {
        set_override(Propagate::Suppress);
    }
    match cmd {
        b"IDX.CREATE" => crate::cmd_index::cmd_idx_create(ctx, store, args, out),
        b"IDX.DROP" => crate::cmd_index::cmd_idx_drop(ctx, args, out),
        b"VIEW.CREATE" => crate::cmd_view::cmd_view_create(ctx, args, out),
        b"VIEW.DROP" => crate::cmd_view::cmd_view_drop(ctx, args, out),
        b"TABLE.DROP" => crate::cmd_table::cmd_table_drop(ctx, args, out),
        _ => crate::cmd_table::cmd_table_local(ctx, cmd, store, args, out),
    }
    true
}

/// Apply a catalog frame from the log, a primary or a snapshot: take it
/// when it is newer than what this node holds, or, on a full sync,
/// whenever it comes from another lineage.
fn adopt<A: ArgvView + ?Sized>(state: &RuntimeState, args: &A, full_sync: bool) -> Option<()> {
    let num = |i: usize| std::str::from_utf8(args.get(i)?).ok()?.parse::<u64>().ok();
    let text = |i: usize| std::str::from_utf8(args.get(i)?).ok();
    let (lineage, version) = (num(1)?, num(2)?);
    let (index, view, table) = (text(3)?, text(4)?, text(5)?);
    // held across the install: shards replay their logs at once, and two
    // frames installed in parts would leave a catalog neither of them holds
    let mut held = state.catalogs.record.lock();
    if (lineage, version) <= *held && !(full_sync && lineage != held.0) {
        return Some(());
    }
    let mut icat = if index.is_empty() { Catalog::new() } else { Catalog::from_sidecar(index)? };
    let vcat = if view.is_empty() { ViewCatalog::new() } else { ViewCatalog::from_sidecar(view)? };
    let tcat =
        if table.is_empty() { TableCatalog::new() } else { TableCatalog::from_sidecar(table)? };
    crate::cmd_index_install::fit_partitions(&mut icat, state.nshards());
    install(state, icat, vcat, tcat);
    *held = (lineage, version);
    Some(())
}

/// Install each catalog that differs from the one held: an unchanged one
/// is left alone, so its indexes are not rebuilt.
fn install(state: &RuntimeState, icat: Catalog, vcat: ViewCatalog, tcat: TableCatalog) {
    let c = &state.catalogs;
    let _held = c.hold();
    let differs = |held: Option<String>, new: String| held.unwrap_or_default() != new;
    let change = CatalogChange {
        index: differs(c.index().map(|x| x.to_sidecar()), icat.to_sidecar()).then_some(icat),
        table: differs(c.table().map(|x| x.to_sidecar()), tcat.to_sidecar()).then_some(tcat),
        view: differs(c.view().map(|x| x.to_sidecar()), vcat.to_sidecar()).then_some(vcat),
    };
    state.install_catalogs(change);
}

/// `XINTERNAL.CATALOG …` from the log or a primary.
pub(crate) fn apply<A: ArgvView + ?Sized>(state: &RuntimeState, args: &A, out: &mut Vec<u8>) {
    match adopt(state, args, false) {
        Some(()) => encode_simple_string(out, "OK"),
        None => encode_error(out, "ERR malformed catalog record"),
    }
}

/// The frame a snapshot or a rewritten log keeps beside the keyspace:
/// one always, an empty catalog included, so a snapshot's lineage says
/// whose catalog it holds. A primary that has recorded nothing mints its
/// lineage here once every shard has restored (a restored frame may
/// still bring one until then); a replica only takes its primary's.
pub(crate) fn snapshot_aux(state: &RuntimeState) -> Argv {
    let mut at = state.catalogs.record.lock();
    let _held = state.catalogs.hold();
    let restored = state.catalogs.record.restored.load(Ordering::Acquire) >= state.nshards();
    if restored && !state.replication.is_replica() {
        mint(&mut at);
    }
    Argv::from(frame(state, *at))
}

/// The frame a loaded snapshot carried, taken as a frame from the log or
/// the stream is: newer wins, so a full sync of one shard, served from a
/// snapshot older than frames another shard already applied, leaves the
/// catalog as they did; a full sync from another lineage (another
/// primary) replaces it, an empty one included. A snapshot with no frame
/// comes from a primary with no replicated catalog (a 6.4 server, or an
/// embedded store that has recorded none), so a replica of it holds none.
pub(crate) fn load_snapshot_aux(state: &RuntimeState, aux: Option<&Argv>, full_sync: bool) {
    match aux {
        Some(frame) => {
            if adopt(state, frame, full_sync).is_none() {
                eprintln!("kevy: a snapshot's catalog record is malformed; catalog left as it was");
            }
        }
        None if full_sync => {
            let mut held = state.catalogs.record.lock();
            if held.0 != 0 {
                install(state, Catalog::new(), ViewCatalog::new(), TableCatalog::new());
                *held = (0, 0);
            }
        }
        None => {}
    }
}

/// A shard finished its startup restore; no shard serves until all have.
/// The last one settles the files a 6.4 directory kept its catalog in:
/// superseded when the log or snapshot held a catalog, otherwise read
/// once and recorded, then removed as soon as the record is on disk.
pub(crate) fn shard_restored(state: &RuntimeState, record: &mut dyn FnMut(&Argv) -> bool) {
    let r = &state.catalogs.record;
    if r.restored.fetch_add(1, Ordering::AcqRel) + 1 != state.nshards() {
        return;
    }
    let dir = state.config().server.data_dir.clone();
    if dir.as_os_str().is_empty() || state.replication.is_replica() {
        return;
    }
    let superseded = r.lock().0 != 0;
    if !superseded && !import(state, &dir, record) {
        return;
    }
    for name in SIDECARS {
        // absent is the usual case
        drop(std::fs::remove_file(dir.join(name)));
    }
}

/// What one sidecar holds: empty when absent, `None` when present but
/// unreadable.
fn read_sidecar<T: Default>(
    dir: &std::path::Path,
    name: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Option<T> {
    match std::fs::read_to_string(dir.join(name)) {
        Ok(text) => parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(T::default()),
        Err(_) => None,
    }
}

/// Install the catalog the sidecars in `dir` hold and record it; `true`
/// once it is on disk in the log (the sidecars are then redundant). A
/// sidecar that does not parse is left in place for someone to look at.
fn import(
    state: &RuntimeState,
    dir: &std::path::Path,
    record: &mut dyn FnMut(&Argv) -> bool,
) -> bool {
    let icat = read_sidecar(dir, SIDECARS[0], Catalog::from_sidecar);
    let vcat = read_sidecar(dir, SIDECARS[1], ViewCatalog::from_sidecar);
    let tcat = read_sidecar(dir, SIDECARS[2], TableCatalog::from_sidecar);
    let (Some(mut icat), Some(vcat), Some(tcat)) = (icat, vcat, tcat) else {
        eprintln!(
            "kevy: a catalog file in {} does not parse; it stays in place, not imported",
            dir.display()
        );
        return false;
    };
    if icat.is_empty() && vcat.is_empty() && tcat.is_empty() {
        return true;
    }
    crate::cmd_index_install::fit_partitions(&mut icat, state.nshards());
    install(state, icat, vcat, tcat);
    let frame = {
        let mut at = state.catalogs.record.lock();
        let _held = state.catalogs.hold();
        next_frame(state, &mut at)
    };
    record(&Argv::from(frame))
}

#[cfg(test)]
mod tests;

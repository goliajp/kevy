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
//! first change a node records, carried by every frame, and adopted by
//! whoever applies one, so a promoted replica goes on from its primary's
//! versions. A full sync replaces a replica's catalog with its primary's
//! even when that one is older, since it comes from another lineage.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use kevy_index::{Catalog, TableCatalog, ViewCatalog};
use kevy_resp::{Argv, ArgvView, encode_error, encode_simple_string};
use kevy_rt::propagation::{Propagate, set_override};
use kevy_store::Store;

use crate::state::{Ctx, RuntimeState};

/// The files the catalog lived in before it was recorded state (6.4 and
/// earlier). Read once, when every shard has restored and neither log nor
/// snapshot held a catalog; removed once one does.
pub(crate) const SIDECARS: [&str; 3] =
    ["index-catalog.meta", "view-catalog.meta", "table-catalog.meta"];

#[derive(Debug, Default)]
pub(crate) struct RecordState {
    /// `(lineage, version)`; lineage 0 = nothing recorded yet.
    at: Mutex<(u64, u64)>,
    restored: AtomicUsize,
}

impl RecordState {
    fn at(&self) -> (u64, u64) {
        *self.at.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// How many catalog installs this state has seen: moves on any change.
fn installs(state: &RuntimeState) -> u64 {
    let c = &state.catalogs;
    c.index_gen().wrapping_add(c.view_gen()).wrapping_add(c.table_gen())
}

/// The current catalog as the frame that records it at `at`.
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

/// Move the catalog to its next version, minting a lineage on the first.
fn next_version(state: &RuntimeState) -> (u64, u64) {
    let mut at = state.catalogs.record.at.lock().unwrap_or_else(PoisonError::into_inner);
    if at.0 == 0 {
        at.0 = kevy_store::now_unix_ms().max(1);
    }
    at.1 += 1;
    *at
}

/// Run a catalog command and record what it changed. A command that
/// changed nothing records nothing, not even its own argv: the catalog is
/// recorded only as its frame. A frame applied from a log or a primary is
/// recorded by the frame itself.
fn recorded<T>(state: &RuntimeState, silent_if_unchanged: bool, run: impl FnOnce() -> T) -> T {
    let before = installs(state);
    let out = run();
    if kevy_rt::applying_record() {
        return out;
    }
    if installs(state) != before {
        let at = next_version(state);
        set_override(Propagate::Replace(frame(state, at)));
    } else if silent_if_unchanged {
        set_override(Propagate::Suppress);
    }
    out
}

/// The catalog commands a client sends; `false` = not one of them.
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
    recorded(ctx.state, true, || match cmd {
        b"IDX.CREATE" => crate::cmd_index::cmd_idx_create(ctx, store, args, out),
        b"IDX.DROP" => crate::cmd_index::cmd_idx_drop(ctx, args, out),
        b"VIEW.CREATE" => crate::cmd_view::cmd_view_create(ctx, args, out),
        b"VIEW.DROP" => crate::cmd_view::cmd_view_drop(ctx, args, out),
        b"TABLE.DROP" => crate::cmd_table::cmd_table_drop(ctx, args, out),
        _ => crate::cmd_table::cmd_table_local(ctx, cmd, store, args, out),
    });
    true
}

/// A fan-out's reduce, recording what it changed in the catalog (a
/// sampled global declaration, a global rebuild's splits, an automatic
/// declaration a refused query earned). A reduce that changed nothing is
/// a read and asks for nothing.
pub(crate) fn reduced<T>(state: &RuntimeState, run: impl FnOnce() -> T) -> T {
    recorded(state, false, run)
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
    let mut held = state.catalogs.record.at.lock().unwrap_or_else(PoisonError::into_inner);
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
    let differs = |held: Option<String>, new: String| held.unwrap_or_default() != new;
    if differs(c.index().map(|x| x.to_sidecar()), icat.to_sidecar()) {
        state.install_index_catalog(icat);
    }
    if differs(c.table().map(|x| x.to_sidecar()), tcat.to_sidecar()) {
        state.install_table_catalog(tcat);
    }
    if differs(c.view().map(|x| x.to_sidecar()), vcat.to_sidecar()) {
        state.install_view_catalog(vcat);
    }
}

/// `XINTERNAL.CATALOG …` from the log or a primary.
pub(crate) fn apply<A: ArgvView + ?Sized>(state: &RuntimeState, args: &A, out: &mut Vec<u8>) {
    match adopt(state, args, false) {
        Some(()) => encode_simple_string(out, "OK"),
        None => encode_error(out, "ERR malformed catalog record"),
    }
}

/// The frame a snapshot or a rewritten log keeps beside the keyspace.
pub(crate) fn snapshot_aux(state: &RuntimeState) -> Option<Argv> {
    let at = state.catalogs.record.at();
    (at.0 != 0).then(|| Argv::from(frame(state, at)))
}

/// The frame a loaded snapshot carried: newer wins at boot; a full sync
/// replaces the catalog with the primary's, an empty one included.
pub(crate) fn load_snapshot_aux(state: &RuntimeState, aux: Option<&Argv>, full_sync: bool) {
    match aux {
        Some(frame) => {
            if adopt(state, frame, full_sync).is_none() {
                eprintln!("kevy: a snapshot's catalog record is malformed; catalog left as it was");
            }
        }
        None if full_sync => {
            let mut held = state.catalogs.record.at.lock().unwrap_or_else(PoisonError::into_inner);
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
    let superseded = r.at().0 != 0;
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
    let at = next_version(state);
    record(&Argv::from(frame(state, at)))
}

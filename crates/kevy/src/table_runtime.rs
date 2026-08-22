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

/// One table's un-packed rows, and how far through them this shard is.
///
/// The index backfill's shape, for the same reason: a declaration has to
/// reach rows that already exist, and there can be two million of them.
pub(crate) struct PackJob {
    names: Vec<Vec<u8>>,
    keys: Vec<Vec<u8>>,
    pos: usize,
}

/// This shard's packing backfill.
#[derive(Default)]
pub(crate) struct PackBackfill {
    /// The table-catalog generation these jobs were built from.
    generation: u64,
    jobs: Vec<PackJob>,
}

/// Keys converted per tick — the index backfill's batch, so the two
/// backfills a declaration starts cost the same per tick.
const BATCH: usize = 2048;

/// Tick hook: pack the rows a declaration did not reach.
///
/// `on_write` only ever sees rows written after the table existed. Three
/// ordinary sequences leave rows behind it: declaring a table over an
/// existing keyspace, restoring from a snapshot (whose loader installs rows
/// without going through the dispatcher), and a row nothing writes again.
/// For a representation whose whole purpose is memory, that is most of the
/// population.
pub(crate) fn on_tick(ctx: &Ctx<'_>, store: &mut Store) {
    if !store.packed_rows_enabled() {
        return;
    }
    let mut bf = ctx.shard.packing.borrow_mut();
    let generation = ctx.state.catalogs.table_gen();
    if bf.generation != generation {
        bf.jobs = collect_jobs(ctx, store);
        bf.generation = generation;
    }
    let Some(job) = bf.jobs.iter_mut().find(|j| j.pos < j.keys.len()) else { return };
    let end = (job.pos + BATCH).min(job.keys.len());
    // Split the borrow: `pack_row` takes the store, the job holds the keys.
    let slice: Vec<Vec<u8>> = job.keys[job.pos..end].to_vec();
    let names = job.names.clone();
    job.pos = end;
    let done = job.pos >= job.keys.len();
    drop(bf);
    for key in &slice {
        store.pack_row(key, &names);
    }
    if done {
        // The key list is the expensive part — a Vec per key, taken while
        // the rows were still unpacked. Drop it as soon as it is spent
        // rather than holding it until the next declaration.
        let mut bf = ctx.shard.packing.borrow_mut();
        bf.jobs.retain(|j| j.pos < j.keys.len());
    }
}

/// Snapshot each declared table's keys on THIS shard. Live writes from now
/// on hit `on_write` first and pack there; `pack_row` is a no-op on a row
/// that is already packed, so the two cannot fight.
fn collect_jobs(ctx: &Ctx<'_>, store: &mut Store) -> Vec<PackJob> {
    let Some(tables) = ctx.state.catalogs.table() else { return Vec::new() };
    tables
        .iter()
        .map(|t| {
            let mut pat = t.prefix.clone();
            pat.push(b'*');
            PackJob {
                names: t.columns.iter().map(|(n, _)| n.clone()).collect(),
                keys: store.collect_keys(Some(&pat), None),
                pos: 0,
            }
        })
        .collect()
}

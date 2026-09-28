//! The `GLOBAL` paths of a `TABLE.DECLARE`: each compiled index the
//! declaration names `GLOBAL` enters the catalog spread over the shards by
//! value, with the split points written after `SPLIT AT` or, without them,
//! taken from a sample of the rows (`crate::cmd_global_sample`) — the same
//! two ways `IDX.CREATE … PARTITION global` takes.

use crate::cmd_index_install::Sampler;
use kevy_index::{Catalog, GlobalPath, IndexSpec, Partitioning, order_key, splits_from_sample};

/// Admit a table's `compiled` indexes into `icat`, the `GLOBAL` ones with
/// their partitioning. `Err` is the wire error.
pub(crate) fn admit(
    icat: &mut Catalog,
    compiled: Vec<IndexSpec>,
    globals: &[GlobalPath],
    sampler: &mut Sampler<'_>,
    nshards: usize,
) -> Result<(), String> {
    for ispec in compiled {
        match globals.iter().find(|g| g.path == ispec.name) {
            None => icat.create(ispec)?,
            Some(g) => {
                let part = partitioning(&ispec, g, sampler, nshards)?;
                icat.create_with(ispec, part)?;
            }
        }
    }
    Ok(())
}

fn partitioning(
    spec: &IndexSpec,
    g: &GlobalPath,
    sampler: &mut Sampler<'_>,
    nshards: usize,
) -> Result<Partitioning, String> {
    let n = nshards.max(1);
    if g.split_at.is_empty() {
        let sample = sampler.sample(spec, n);
        return Ok(Partitioning::Global { splits: splits_from_sample(sample, n) });
    }
    Ok(Partitioning::Global { splits: explicit_splits(spec, g, n)? })
}

/// The `SPLIT AT` values in the path's order encoding.
fn explicit_splits(spec: &IndexSpec, g: &GlobalPath, n: usize) -> Result<Vec<Vec<u8>>, String> {
    if spec.composite.is_some() {
        return Err("ERR SPLIT AT applies to an INDEX path; an ORDERPATH declared GLOBAL samples its split points".into());
    }
    if g.split_at.len() >= n {
        return Err("ERR SPLIT AT allows at most one point fewer than the shard count".into());
    }
    g.split_at
        .iter()
        .map(|raw| {
            order_key(spec.ty, raw)
                .ok_or_else(|| "ERR SPLIT AT value does not coerce to the column type".into())
        })
        .collect()
}

/// Whether the paths `names` are spread in `icat` as `globals` asks — the
/// `TABLE.ENSURE` question. A `GLOBAL` without `SPLIT AT` matches whatever
/// split points the path was sampled with.
pub(crate) fn same_spread(icat: &Catalog, names: &[Vec<u8>], globals: &[GlobalPath]) -> bool {
    names.iter().all(|name| {
        let cur = icat.partitioning(name);
        match globals.iter().find(|g| g.path == *name) {
            None => !cur.is_global(),
            Some(g) if g.split_at.is_empty() => cur.is_global(),
            Some(g) => {
                let Some((spec, _)) = icat.get(name) else { return false };
                let want = explicit_splits(spec, g, usize::MAX).ok();
                matches!((cur, want), (Partitioning::Global { splits }, Some(w)) if *splits == w)
            }
        }
    })
}

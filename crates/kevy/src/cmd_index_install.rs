//! The end of IDX.CREATE: the `PARTITION` / `SPLIT` options, and installing
//! the new index into the catalog every shard reads. Split from
//! `cmd_index.rs` for the 500-line cap.

use std::collections::HashMap;

use kevy_index::{IndexSpec, Partitioning, parse_split_point, splits_from_weighted};
use kevy_resp::{ArgvView, encode_error};
use kevy_store::Store;

use crate::index_runtime::{self, POINTS_PER_PARTITION};
use crate::state::Ctx;

/// The partitioning an IDX.CREATE asked for, before its split values are
/// encoded in the index's order.
#[derive(Default)]
pub(crate) struct PartitionOpt {
    pub(crate) global: bool,
    pub(crate) split: Vec<Vec<u8>>,
}

/// Where a sampled global index's split points come from.
pub(crate) enum Sampler<'a> {
    /// The rows of the shard running the command.
    Shard(&'a mut Store),
    /// What every shard sent in the first phase of the two-phase form:
    /// rank buckets per index name, and whether any shard's tiering floor
    /// refuses a new index.
    Gathered { samples: &'a HashMap<Vec<u8>, Vec<(Vec<u8>, u64)>>, tier_blocked: bool },
}

impl Sampler<'_> {
    /// Rank buckets of `spec`'s values, [`POINTS_PER_PARTITION`] per
    /// partition of `nshards` from each shard that sent them.
    pub(crate) fn sample(&mut self, spec: &IndexSpec, nshards: usize) -> Vec<(Vec<u8>, u64)> {
        match self {
            Sampler::Shard(store) => {
                index_runtime::quantile_points(store, spec, POINTS_PER_PARTITION * nshards)
            }
            Sampler::Gathered { samples, .. } => {
                samples.get(&spec.name).cloned().unwrap_or_default()
            }
        }
    }

    /// Whether the tiering floor refuses a new index.
    pub(crate) fn tier_blocked(&self) -> bool {
        match self {
            Sampler::Shard(store) => store.tier_index_floor_blocked(0),
            Sampler::Gathered { tier_blocked, .. } => *tier_blocked,
        }
    }
}

/// `PARTITION local|global` or `SPLIT v`: `None` when `opt` is neither, so
/// the caller goes on to its other options; `Some(Err(()))` when the reply
/// already holds an error.
pub(crate) fn apply_partition_opt(
    opt: &[u8],
    val: &[u8],
    p: &mut PartitionOpt,
    out: &mut Vec<u8>,
) -> Option<Result<(), ()>> {
    if opt.eq_ignore_ascii_case(b"PARTITION") {
        p.global = if val.eq_ignore_ascii_case(b"GLOBAL") {
            true
        } else if val.eq_ignore_ascii_case(b"LOCAL") {
            false
        } else {
            encode_error(out, "ERR PARTITION must be local|global");
            return Some(Err(()));
        };
        return Some(Ok(()));
    }
    if opt.eq_ignore_ascii_case(b"SPLIT") {
        p.split.push(val.to_vec());
        return Some(Ok(()));
    }
    None
}

/// The option keywords this module reads, for the variadic lists that
/// collect up to the next keyword.
pub(crate) fn is_partition_opt(a: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b"PARTITION") || a.eq_ignore_ascii_case(b"SPLIT")
}

/// Encode the split values in the index's order and hold them to the
/// shard count: `P - 1` points make `P` partitions, at most one per shard.
/// A global index given no split points takes the shard count's quantiles
/// of `sampler`'s sample (none when there are no rows yet).
fn partitioning(
    p: PartitionOpt,
    sampler: &mut Sampler<'_>,
    spec: &IndexSpec,
    nshards: usize,
    out: &mut Vec<u8>,
) -> Result<Partitioning, ()> {
    if p.global && p.split.is_empty() {
        let n = nshards.max(1);
        return Ok(Partitioning::Global {
            splits: splits_from_weighted(sampler.sample(spec, n), n),
        });
    }
    if !p.global {
        if !p.split.is_empty() {
            encode_error(out, "ERR SPLIT requires PARTITION global");
            return Err(());
        }
        return Ok(Partitioning::Local);
    }
    if p.split.len() >= nshards.max(1) {
        encode_error(out, "ERR SPLIT allows at most one point fewer than the shard count");
        return Err(());
    }
    let mut splits = Vec::with_capacity(p.split.len());
    for raw in &p.split {
        let Some(enc) = parse_split_point(spec, raw) else {
            encode_error(out, "ERR SPLIT value does not coerce to the index TYPE");
            return Err(());
        };
        splits.push(enc);
    }
    Ok(Partitioning::Global { splits })
}

/// `IDX.CREATE` with the split points of a sampled global index taken
/// from `sampler`: this shard's rows, or every shard's (the two-phase
/// form, `crate::cmd_global_sample`).
pub(crate) fn create<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    sampler: &mut Sampler<'_>,
    args: &A,
    out: &mut Vec<u8>,
) {
    let Some((spec, part)) = crate::cmd_index::parse_create(args, out) else {
        return;
    };
    if sampler.tier_blocked() {
        return encode_error(out, crate::cmd_index::TIER_FLOOR_REFUSAL);
    }
    install_new_index(ctx, sampler, spec, part, out);
}

/// Clone the catalog, add `spec` with its partitioning, and on success
/// persist + install it.
pub(crate) fn install_new_index(
    ctx: &Ctx<'_>,
    sampler: &mut Sampler<'_>,
    spec: IndexSpec,
    part: PartitionOpt,
    out: &mut Vec<u8>,
) {
    let Ok(partitioning) = partitioning(part, sampler, &spec, ctx.state.nshards(), out) else {
        return;
    };
    let mut cat = ctx.state.catalogs.index().map(|c| (*c).clone()).unwrap_or_default();
    match cat.create_with(spec, partitioning) {
        Ok(()) => {
            crate::cmd_index::persist_sidecar(ctx.state.sidecar_dir(), &cat);
            ctx.state.install_index_catalog(cat);
            out.extend_from_slice(b"+OK\r\n");
        }
        Err(e) => encode_error(out, e),
    }
}

/// A global index persisted with more partitions than there are shards now
/// keeps `n - 1` of its split points, evenly spread, so each shard still
/// owns at most one partition. Whether any index changed.
pub(crate) fn fit_partitions(cat: &mut kevy_index::Catalog, n: usize) -> bool {
    let over: Vec<(Vec<u8>, Vec<Vec<u8>>)> = cat
        .iter()
        .filter_map(|(spec, _)| match cat.partitioning(&spec.name) {
            kevy_index::Partitioning::Global { splits } if splits.len() >= n.max(1) => {
                Some((spec.name.clone(), splits.clone()))
            }
            _ => None,
        })
        .collect();
    for (name, splits) in &over {
        let points = splits.iter().map(|s| (s.clone(), 1)).collect();
        cat.set_splits(name, splits_from_weighted(points, n.max(1)));
    }
    !over.is_empty()
}

#[cfg(test)]
mod tests {
    use kevy_index::{Catalog, IndexKind, IndexSpec, Partitioning, ValType};

    #[test]
    fn a_catalog_from_more_shards_keeps_one_partition_per_shard() {
        let spec = |name: &[u8]| {
            IndexSpec::single_field(
                name.to_vec(),
                b"u:".to_vec(),
                b"a".to_vec(),
                ValType::Str,
                IndexKind::Range,
            )
        };
        let splits: Vec<Vec<u8>> = (1..8u8).map(|b| vec![b]).collect();
        let mut cat = Catalog::new();
        cat.create_with(spec(b"wide"), Partitioning::Global { splits }).unwrap();
        cat.create_with(spec(b"narrow"), Partitioning::Global { splits: vec![vec![5]] }).unwrap();
        assert!(super::fit_partitions(&mut cat, 4));
        assert_eq!(cat.partitioning(b"wide").partitions(), 4);
        let Partitioning::Global { splits } = cat.partitioning(b"wide") else { unreachable!() };
        // eight even partitions merged two by two
        assert_eq!(splits, &[vec![2], vec![4], vec![6]]);
        assert_eq!(cat.partitioning(b"narrow").partitions(), 2, "one that fits is left alone");
        assert!(!super::fit_partitions(&mut cat, 4), "and a fitted catalog stays");
    }
}

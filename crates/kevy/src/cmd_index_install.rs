//! The end of IDX.CREATE: the `PARTITION` / `SPLIT` options, and installing
//! the new index into the catalog every shard reads. Split from
//! `cmd_index.rs` for the 500-line cap.

use kevy_index::{IndexSpec, Partitioning, ValType, order_key};

use kevy_resp::encode_error;

use crate::state::Ctx;

/// The partitioning an IDX.CREATE asked for, before its split values are
/// encoded in the index's order.
#[derive(Default)]
pub(crate) struct PartitionOpt {
    global: bool,
    split: Vec<Vec<u8>>,
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
fn partitioning(
    p: PartitionOpt,
    ty: ValType,
    nshards: usize,
    out: &mut Vec<u8>,
) -> Result<Partitioning, ()> {
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
        let Some(enc) = order_key(ty, raw) else {
            encode_error(out, "ERR SPLIT value does not coerce to the index TYPE");
            return Err(());
        };
        splits.push(enc);
    }
    Ok(Partitioning::Global { splits })
}

/// Clone the catalog, add `spec` with its partitioning, and on success
/// persist + install it.
pub(crate) fn install_new_index(
    ctx: &Ctx<'_>,
    spec: IndexSpec,
    part: PartitionOpt,
    out: &mut Vec<u8>,
) {
    let Ok(partitioning) = partitioning(part, spec.ty, ctx.state.nshards(), out) else {
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

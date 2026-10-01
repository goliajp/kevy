//! The extension fan-out's two halves for every family of extension
//! verbs: which family's per-shard op runs, and which reduce folds the
//! chunks at the origin.

use kevy_rt::ExtensionReduced;
use kevy_store::Store;

use crate::state::Ctx;

fn has_prefix(argv: &[Vec<u8>], p: &[u8]) -> bool {
    argv.first().is_some_and(|v| v.len() > p.len() && v[..p.len()].eq_ignore_ascii_case(p))
}

fn is(argv: &[Vec<u8>], verb: &[u8]) -> bool {
    argv.first().is_some_and(|v| v.eq_ignore_ascii_case(verb))
}

/// `Commands::extension_op`.
pub(crate) fn op(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    if argv.first().is_some_and(|v| crate::cmd_global_sample::is_verb(v)) {
        return crate::cmd_global_sample::op(ctx, store, argv);
    }
    if is(argv, b"PREFIX.DIGEST") {
        return crate::cmd_digest::extension_op(store, argv);
    }
    if has_prefix(argv, b"VIEW.") {
        return crate::cmd_view::extension_op(ctx, store, argv);
    }
    if has_prefix(argv, b"TABLE.") {
        return crate::cmd_table::extension_op(ctx, store, argv);
    }
    crate::cmd_index_query::extension_op(ctx, store, argv)
}

/// `Commands::extension_reduce`.
pub(crate) fn reduce(
    ctx: &Ctx<'_>,
    argv: &[Vec<u8>],
    chunks: Vec<Vec<u8>>,
    proto: kevy_resp::RespVersion,
) -> ExtensionReduced {
    let catalogs = &ctx.state.catalogs;
    let reduced = if argv.first().is_some_and(|v| crate::cmd_global_sample::is_verb(v)) {
        ExtensionReduced::Reply(crate::cmd_global_sample::reduce(ctx, argv, &chunks))
    } else if is(argv, b"PREFIX.DIGEST") {
        ExtensionReduced::Reply(crate::cmd_digest::extension_reduce(chunks))
    } else if has_prefix(argv, b"VIEW.") {
        crate::cmd_view::extension_reduce(catalogs, argv, chunks)
    } else if has_prefix(argv, b"TABLE.") {
        crate::cmd_table::extension_reduce(catalogs, argv, chunks)
    } else {
        crate::cmd_index_reduce::extension_reduce(ctx.state, argv, chunks)
    };
    match reduced {
        ExtensionReduced::Reply(reply) if proto == kevy_resp::RespVersion::V3 => {
            ExtensionReduced::Reply(crate::cmd_index_reduce::resp3_upgrade(argv, reply))
        }
        other => other,
    }
}

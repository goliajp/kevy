//! VIEW.* command surface. CREATE/DROP are Local catalog
//! mutations (sidecar-persisted, like IDX.*); QUERY/LIST/VERIFY/
//! REBUILD/EXPLAIN ride the extension fan-out.
//!
//! Tree grammar over argv (parens are separate arguments):
//! `( AND|OR|DIFF <sub> <sub> )` | `<index> RANGE <min> <max>` |
//! `<index> EQ <v>`.

use kevy_resp::CmdError;
use std::path::Path;

use kevy_index::{Catalog, IndexValue, Leaf, Tree, ViewCatalog, ViewMode, ViewSpec};
use kevy_resp::{ArgvView, encode_error, encode_integer};
use kevy_store::Store;

use crate::cmd_index_query::{ST_BUILDING, ST_NOINDEX, ST_OK, encode_value};
use crate::state::{Ctx, RuntimeState};
use crate::view_runtime;

const SIDECAR: &str = "view-catalog.meta";

/// Boot: load the persisted view catalog (runs after
/// `cmd_index::boot` — view leaves reference index specs).
pub(crate) fn boot(state: &RuntimeState) {
    let Some(dir) = state.sidecar_dir() else { return };
    if let Ok(text) = std::fs::read_to_string(dir.join(SIDECAR))
        && let Some(cat) = ViewCatalog::from_sidecar(&text)
        && !cat.is_empty()
    {
        state.install_view_catalog(cat);
    }
}

fn persist_sidecar(dir: Option<&Path>, cat: &ViewCatalog) {
    let Some(dir) = dir else { return };
    let tmp = dir.join("view-catalog.meta.tmp");
    if std::fs::write(&tmp, cat.to_sidecar()).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join(SIDECAR));
    }
}

/// Parse one tree node starting at `i`; returns `(tree, next_i)`.
/// `icat` is the index-catalog snapshot leaf lookups resolve against.
fn parse_tree<A: ArgvView + ?Sized>(
    icat: Option<&Catalog>,
    args: &A,
    i: usize,
    depth: usize,
) -> Result<(Tree, usize), CmdError> {
    if depth > kevy_index::MAX_TREE_DEPTH {
        return Err(CmdError::Wire("ERR view tree deeper than 3"));
    }
    let tok = args.get(i).ok_or("ERR truncated view tree")?;
    if tok == b"(" {
        let op = args.get(i + 1).ok_or("ERR truncated view tree")?;
        let (a, ni) = parse_tree(icat, args, i + 2, depth + 1)?;
        let (b, ni) = parse_tree(icat, args, ni, depth + 1)?;
        if args.get(ni).map(|t| t as &[u8]) != Some(b")") {
            return Err(CmdError::Wire("ERR expected ) in view tree"));
        }
        let tree = if op.eq_ignore_ascii_case(b"AND") {
            Tree::And(Box::new(a), Box::new(b))
        } else if op.eq_ignore_ascii_case(b"OR") {
            Tree::Or(Box::new(a), Box::new(b))
        } else if op.eq_ignore_ascii_case(b"DIFF") {
            Tree::Diff(Box::new(a), Box::new(b))
        } else {
            return Err(CmdError::Wire("ERR view tree op must be AND|OR|DIFF"));
        };
        Ok((tree, ni + 1))
    } else {
        // leaf: <index> RANGE min max | <index> EQ v — bounds coerced
        // per the index's declared type.
        let index = tok.to_vec();
        let spec_ty = icat
            .and_then(|c| c.get(&index).map(|(s, _)| s.ty))
            .ok_or("ERR view leaf references unknown index")?;
        let shape = args.get(i + 1).ok_or("ERR truncated view leaf")?;
        if shape.eq_ignore_ascii_case(b"RANGE") {
            let min = IndexValue::parse_literal(
                spec_ty,
                args.get(i + 2).ok_or("ERR truncated view leaf")?,
            )
            .ok_or("ERR leaf min does not coerce to the index type")?;
            let max = IndexValue::parse_literal(
                spec_ty,
                args.get(i + 3).ok_or("ERR truncated view leaf")?,
            )
            .ok_or("ERR leaf max does not coerce to the index type")?;
            Ok((Tree::Leaf(Leaf { index, min, max }), i + 4))
        } else if shape.eq_ignore_ascii_case(b"EQ") {
            let v = IndexValue::parse_literal(
                spec_ty,
                args.get(i + 2).ok_or("ERR truncated view leaf")?,
            )
            .ok_or("ERR leaf value does not coerce to the index type")?;
            Ok((Tree::Leaf(Leaf { index, min: v.clone(), max: v }), i + 3))
        } else {
            Err(CmdError::Wire("ERR view leaf shape must be RANGE|EQ"))
        }
    }
}

/// `VIEW.CREATE <name> QUERY <tree…> ORDER BY <index> [DESC]
/// [MODE virtual|materialized] [TOPK k] [VIA tpl]`.
pub(crate) fn cmd_view_create<A: ArgvView + ?Sized>(ctx: &Ctx<'_>, args: &A, out: &mut Vec<u8>) {
    if args.len() < 8 || !args[2].eq_ignore_ascii_case(b"QUERY") {
        return encode_error(
            out,
            "ERR usage: VIEW.CREATE name QUERY <tree> ORDER BY idx [DESC] [MODE v|m] [TOPK k] [VIA tpl]",
        );
    }
    let icat = ctx.state.catalogs.index();
    let (tree, mut i) = match parse_tree(icat.as_deref(), args, 3, 1) {
        Ok(t) => t,
        Err(e) => return encode_error(out, e.as_wire()),
    };
    if !(args.get(i).is_some_and(|t| t.eq_ignore_ascii_case(b"ORDER"))
        && args.get(i + 1).is_some_and(|t| t.eq_ignore_ascii_case(b"BY")))
    {
        return encode_error(out, "ERR ORDER BY <index> is required");
    }
    let Some(order_by) = args.get(i + 2).map(|t| t.to_vec()) else {
        return encode_error(out, "ERR ORDER BY <index> is required");
    };
    if icat.as_deref().and_then(|c| c.get(&order_by).map(|_| ())).is_none() {
        return encode_error(out, "ERR ORDER BY references unknown index");
    }
    i += 3;
    let (desc, mut mode, top_k, via) = match parse_create_opts(args, i) {
        Ok(opts) => opts,
        Err(e) => return encode_error(out, e.as_wire()),
    };
    if let ViewMode::Materialized { .. } = mode {
        mode = ViewMode::Materialized { top_k };
    } else if top_k != 0 {
        return encode_error(out, "ERR TOPK requires MODE materialized");
    }
    let spec = ViewSpec { name: args[1].to_vec(), tree, order_by, desc, mode, via };
    let mut cat = ctx.state.catalogs.view().map(|c| (*c).clone()).unwrap_or_default();
    match cat.create(spec) {
        Ok(()) => {
            persist_sidecar(ctx.state.sidecar_dir(), &cat);
            ctx.state.install_view_catalog(cat);
            out.extend_from_slice(b"+OK\r\n");
        }
        Err(e) => encode_error(out, e),
    }
}

/// `(desc, mode, top_k, via)` from the optional `VIEW.CREATE` tail.
type CreateOpts = (bool, ViewMode, u32, Option<Vec<u8>>);

/// Parse the optional `VIEW.CREATE` tail starting at `i`:
/// `[DESC] [MODE virtual|materialized] [TOPK k] [VIA tpl]`.
fn parse_create_opts<A: ArgvView + ?Sized>(args: &A, mut i: usize) -> Result<CreateOpts, CmdError> {
    let mut desc = false;
    let mut mode = ViewMode::Virtual;
    let mut top_k = 0u32;
    let mut via = None;
    while i < args.len() {
        let t = &args[i];
        if t.eq_ignore_ascii_case(b"DESC") {
            desc = true;
            i += 1;
        } else if t.eq_ignore_ascii_case(b"MODE") {
            let m = match args.get(i + 1) {
                Some(m) => m,
                None => return Err(CmdError::Wire("ERR MODE requires virtual|materialized")),
            };
            mode = if m.eq_ignore_ascii_case(b"virtual") {
                ViewMode::Virtual
            } else if m.eq_ignore_ascii_case(b"materialized") {
                ViewMode::Materialized { top_k: 0 }
            } else {
                return Err(CmdError::Wire("ERR MODE must be virtual|materialized"));
            };
            i += 2;
        } else if t.eq_ignore_ascii_case(b"TOPK") {
            top_k = match args
                .get(i + 1)
                .and_then(|v| std::str::from_utf8(v).ok())
                .and_then(|s| s.parse().ok())
            {
                Some(k) => k,
                None => return Err(CmdError::Wire("ERR TOPK must be an integer")),
            };
            i += 2;
        } else if t.eq_ignore_ascii_case(b"VIA") {
            via = args.get(i + 1).map(|v| v.to_vec());
            if via.is_none() {
                return Err(CmdError::Wire("ERR VIA requires a template"));
            }
            i += 2;
        } else {
            return Err(CmdError::Wire("ERR syntax error"));
        }
    }
    Ok((desc, mode, top_k, via))
}

/// `VIEW.DROP <name>`.
pub(crate) fn cmd_view_drop<A: ArgvView + ?Sized>(ctx: &Ctx<'_>, args: &A, out: &mut Vec<u8>) {
    if args.len() != 2 {
        return encode_error(out, "ERR usage: VIEW.DROP name");
    }
    let mut cat = ctx.state.catalogs.view().map(|c| (*c).clone()).unwrap_or_default();
    let hit = cat.drop_view(&args[1]);
    if hit {
        persist_sidecar(ctx.state.sidecar_dir(), &cat);
        ctx.state.install_view_catalog(cat);
    }
    encode_integer(out, i64::from(hit));
}

// ---------- extension fan-out ----------

/// Per-shard half for VIEW.QUERY / VIEW.LIST / VIEW.VERIFY /
/// VIEW.REBUILD / VIEW.EXPLAIN.
pub(crate) fn extension_op(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let verb = argv.first().map(Vec::as_slice).unwrap_or(b"");
    if verb.eq_ignore_ascii_case(b"VIEW.QUERY") {
        return op_query(ctx, store, argv);
    }
    if verb.eq_ignore_ascii_case(b"VIEW.LIST") {
        return vec![ST_OK]; // catalog is shared — the reduce renders it
    }
    if verb.eq_ignore_ascii_case(b"VIEW.VERIFY") {
        return op_stats(ctx, store, argv, verb);
    }
    if verb.eq_ignore_ascii_case(b"VIEW.REBUILD") {
        if let Some(name) = argv.get(1) {
            view_runtime::schedule_rebuild(ctx.shard, name);
            view_runtime::on_tick(ctx, store); // run it now on this shard
        }
        return vec![ST_OK];
    }
    if verb.eq_ignore_ascii_case(b"VIEW.EXPLAIN") {
        return op_explain(ctx, store, argv);
    }
    if verb.eq_ignore_ascii_case(b"VIEW.HYDRATE") {
        return op_hydrate(store, argv);
    }
    vec![ST_NOINDEX]
}

/// Phase-2 per-shard: `argv = [verb, cursor, nfields, f…, (member,
/// order, target)*]` — read `f…` from every TARGET this shard owns.
/// Chunk: `(row_idx: u32, (flen|MAX, bytes)*)*`.
fn op_hydrate(store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some(nf) = argv
        .get(2)
        .and_then(|b| b.get(..4))
        .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")) as usize)
    else {
        return vec![crate::cmd_index_query::ST_BADARGS];
    };
    let fields = &argv[3..3 + nf];
    let rows = &argv[3 + nf..];
    // No shard-identity check needed: each shard's store holds only
    // its own keys, so "target present here" IS ownership. Missing
    // targets are nobody's row — the reduce fills them with nils
    // (RFC: target missing = nil). The existence probe is stage-1
    // metadata only — a cold target pays no pread here.
    let present: Vec<(usize, &[u8])> = rows
        .chunks(3)
        .enumerate()
        .filter_map(|(row_idx, row)| {
            let [_member, _order, target] = row else { return None };
            (store.exists(&[target.as_slice()]) > 0).then_some((row_idx, target.as_slice()))
        })
        .collect();
    // This shard's target rows hydrate as ONE batched page — cold
    // rows coalesce into one submission, one decode per row, no
    // promotion, no gate advancement.
    let keys: Vec<&[u8]> = present.iter().map(|(_, t)| *t).collect();
    let prefetched = crate::cmd_index_query::peek_hydration(store, &keys, fields);
    let mut chunk = vec![ST_OK];
    let mut body = Vec::new();
    for ((row_idx, _), row) in present.iter().zip(&prefetched) {
        body.extend_from_slice(&(*row_idx as u32).to_le_bytes());
        let vals = match row {
            Ok(Some(vals)) => vals.as_slice(),
            _ => &[],
        };
        for i in 0..fields.len() {
            match vals.get(i).and_then(Option::as_deref) {
                Some(v) => {
                    body.extend_from_slice(&(v.len() as u32).to_le_bytes());
                    body.extend_from_slice(v);
                }
                None => body.extend_from_slice(&u32::MAX.to_le_bytes()),
            }
        }
    }
    chunk.extend_from_slice(&(present.len() as u32).to_le_bytes());
    chunk.extend_from_slice(&body);
    chunk
}

fn op_query(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some(q) = QueryArgs::parse(argv) else {
        return vec![crate::cmd_index_query::ST_BADARGS];
    };
    match view_runtime::shard_page(ctx, store, &q.name, q.after.as_ref(), q.limit) {
        Ok(rows) => {
            let mut chunk = vec![ST_OK];
            chunk.extend_from_slice(&(rows.len() as u32).to_le_bytes());
            for (v, k) in &rows {
                chunk.extend_from_slice(&(k.len() as u32).to_le_bytes());
                chunk.extend_from_slice(k);
                encode_value(&mut chunk, v);
                chunk.push(0); // no hydration fields at the shard level (VIA = step 3)
            }
            chunk
        }
        Err(e) if e.as_wire().starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(_) => vec![ST_NOINDEX],
    }
}

fn op_stats(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>], _verb: &[u8]) -> Vec<u8> {
    let Some(name) = argv.get(1) else {
        return vec![crate::cmd_index_query::ST_BADARGS];
    };
    match view_runtime::shard_stats(ctx, store, name) {
        Ok((members, bytes, excluded, building)) => {
            let mut chunk = vec![ST_OK];
            chunk.push(u8::from(building));
            chunk.extend_from_slice(&members.to_le_bytes());
            chunk.extend_from_slice(&bytes.to_le_bytes());
            chunk.extend_from_slice(&excluded.to_le_bytes());
            chunk
        }
        Err(_) => vec![ST_NOINDEX],
    }
}

fn op_explain(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some(name) = argv.get(1) else {
        return vec![crate::cmd_index_query::ST_BADARGS];
    };
    let Some(spec) = ctx.state.catalogs.view().and_then(|c| c.get(name).cloned()) else {
        return vec![ST_NOINDEX];
    };
    // Per-leaf local cardinalities.
    let counts = crate::index_runtime::with_segment_resolver(ctx, store, |seg| {
        let mut counts = Vec::new();
        spec.tree.each_leaf(&mut |l| {
            let n = seg(&l.index).map_or(0, |s| s.count(&l.min, &l.max));
            counts.push(n);
        });
        counts
    });
    let mut chunk = vec![ST_OK];
    chunk.push(counts.len() as u8);
    for c in counts {
        chunk.extend_from_slice(&c.to_le_bytes());
    }
    chunk
}

/// `VIEW.QUERY <name> [LIMIT n] [CURSOR c] [FIELDS f…]` — FIELDS
/// requires the view to declare VIA (targets are template-derived).
pub(crate) struct QueryArgs {
    pub name: Vec<u8>,
    pub limit: usize,
    pub after: Option<(IndexValue, Vec<u8>)>,
    pub fields: Vec<Vec<u8>>,
}

impl QueryArgs {
    pub(crate) fn parse(argv: &[Vec<u8>]) -> Option<QueryArgs> {
        let name = argv.get(1)?.clone();
        let mut limit = 100usize;
        let mut after = None;
        let mut fields = Vec::new();
        let mut i = 2;
        while i < argv.len() {
            let t = &argv[i];
            if t.eq_ignore_ascii_case(b"LIMIT") {
                limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                i += 2;
            } else if t.eq_ignore_ascii_case(b"CURSOR") {
                let raw = argv.get(i + 1)?;
                if raw != b"0" {
                    after = crate::cmd_index_query::decode_view_cursor(raw);
                    after.as_ref()?;
                }
                i += 2;
            } else if t.eq_ignore_ascii_case(b"FIELDS") {
                fields = argv[i + 1..].to_vec();
                if fields.is_empty() {
                    return None;
                }
                break;
            } else {
                return None;
            }
        }
        Some(QueryArgs { name, limit: limit.clamp(1, 10_000), after, fields })
    }
}

/// Origin reduce for VIEW.* verbs lives in [`crate::cmd_view_reduce`];
/// re-exported so callers keep the `cmd_view::extension_reduce` path.
pub(crate) use crate::cmd_view_reduce::extension_reduce;

#[cfg(test)]
mod hydrate_tests {
    //! `op_hydrate` on cold rows — byte-identical to the hot twin,
    //! zero promotions, one record read per cold row, one batch per
    //! page (the VIEW.QUERY+VIA server harness is disproportionate for
    //! this; the reduce/fan-out layers above are covered by the
    //! existing view e2e, and this pins the shard half's tier
    //! behavior).

    use super::op_hydrate;
    use kevy_store::Store;

    fn argv(fields: &[&[u8]], rows: &[(&[u8], &[u8], &[u8])]) -> Vec<Vec<u8>> {
        let mut v = vec![b"VIEW.HYDRATE".to_vec(), b"0".to_vec()];
        v.push((fields.len() as u32).to_le_bytes().to_vec());
        v.extend(fields.iter().map(|f| f.to_vec()));
        for (m, o, t) in rows {
            v.push(m.to_vec());
            v.push(o.to_vec());
            v.push(t.to_vec());
        }
        v
    }

    #[test]
    fn cold_rows_hydrate_byte_identical_without_promoting() {
        let d = kevy_tmpdir::TmpDir::new("view-hydrate-cold");
        let mut s = Store::new();
        s.enable_tiering(d.path(), 1 << 30).unwrap();
        for i in 0..3u8 {
            let big = vec![b'v'; 80];
            s.hset(
                format!("t:{i}").as_bytes(),
                &[
                    (b"a".as_slice(), big.as_slice()),
                    (b"b".as_slice(), b"small".as_slice()),
                    (b"c".as_slice(), b"x".as_slice()),
                ],
            )
            .unwrap();
        }
        let a = argv(
            &[b"a", b"b", b"missing"],
            &[
                (b"m0", b"o0", b"t:0"),
                (b"m1", b"o1", b"absent-target"),
                (b"m2", b"o2", b"t:1"),
                (b"m3", b"o3", b"t:2"),
            ],
        );
        let hot = op_hydrate(&mut s, &a);
        for i in 0..3u8 {
            assert!(s.debug_force_demote(format!("t:{i}").as_bytes()));
        }
        let before = s.tier_stats();
        let cold = op_hydrate(&mut s, &a);
        let after = s.tier_stats();
        assert_eq!(cold, hot, "cold hydration must be byte-identical to the hot twin");
        assert_eq!(after.promotions_total, 0, "a hydrated page is not an access signal");
        assert_eq!(after.peek_preads_total - before.peek_preads_total, 3, "one read per cold ROW");
        assert_eq!(
            after.batch_submissions_total - before.batch_submissions_total,
            1,
            "one batch per page"
        );
        // Rows are still cold; the client 2-touch gate starts fresh.
        let again = op_hydrate(&mut s, &a);
        assert_eq!(again, hot);
        assert_eq!(s.tier_stats().promotions_total, 0);
    }
}

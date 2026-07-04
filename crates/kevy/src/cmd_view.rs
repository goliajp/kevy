//! v2.6 — VIEW.* command surface. CREATE/DROP are Local catalog
//! mutations (sidecar-persisted, like IDX.*); QUERY/LIST/VERIFY/
//! REBUILD/EXPLAIN ride the extension fan-out.
//!
//! Tree grammar over argv (parens are separate arguments):
//! `( AND|OR|DIFF <sub> <sub> )` | `<index> RANGE <min> <max>` |
//! `<index> EQ <v>`.

use std::path::Path;

use kevy_index::{IndexValue, Leaf, Tree, ViewCatalog, ViewMode, ViewSpec};
use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer};
use kevy_store::Store;

use crate::cmd_index_query::{ST_BUILDING, ST_NOINDEX, ST_OK, encode_value};
use crate::{index_runtime, view_runtime};

const SIDECAR: &str = "view-catalog.meta";

/// Boot: load the persisted view catalog (data dir already known to
/// `cmd_index::boot`, which runs first and stores it).
pub(crate) fn boot(data_dir: &Path) {
    if let Ok(text) = std::fs::read_to_string(data_dir.join(SIDECAR))
        && let Some(cat) = ViewCatalog::from_sidecar(&text)
        && !cat.is_empty()
    {
        view_runtime::install_catalog(cat);
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
fn parse_tree<A: ArgvView + ?Sized>(args: &A, i: usize, depth: usize) -> Result<(Tree, usize), &'static str> {
    if depth > kevy_index::MAX_TREE_DEPTH {
        return Err("ERR view tree deeper than 3");
    }
    let tok = args.get(i).ok_or("ERR truncated view tree")?;
    if tok == b"(" {
        let op = args.get(i + 1).ok_or("ERR truncated view tree")?;
        let (a, ni) = parse_tree(args, i + 2, depth + 1)?;
        let (b, ni) = parse_tree(args, ni, depth + 1)?;
        if args.get(ni).map(|t| t as &[u8]) != Some(b")") {
            return Err("ERR expected ) in view tree");
        }
        let tree = if op.eq_ignore_ascii_case(b"AND") {
            Tree::And(Box::new(a), Box::new(b))
        } else if op.eq_ignore_ascii_case(b"OR") {
            Tree::Or(Box::new(a), Box::new(b))
        } else if op.eq_ignore_ascii_case(b"DIFF") {
            Tree::Diff(Box::new(a), Box::new(b))
        } else {
            return Err("ERR view tree op must be AND|OR|DIFF");
        };
        Ok((tree, ni + 1))
    } else {
        // leaf: <index> RANGE min max | <index> EQ v — bounds coerced
        // per the index's declared type.
        let index = tok.to_vec();
        let spec_ty = index_runtime::catalog()
            .and_then(|c| c.get(&index).map(|(s, _)| s.ty))
            .ok_or("ERR view leaf references unknown index")?;
        let shape = args.get(i + 1).ok_or("ERR truncated view leaf")?;
        if shape.eq_ignore_ascii_case(b"RANGE") {
            let min = IndexValue::parse_literal(spec_ty, args.get(i + 2).ok_or("ERR truncated view leaf")?)
                .ok_or("ERR leaf min does not coerce to the index type")?;
            let max = IndexValue::parse_literal(spec_ty, args.get(i + 3).ok_or("ERR truncated view leaf")?)
                .ok_or("ERR leaf max does not coerce to the index type")?;
            Ok((Tree::Leaf(Leaf { index, min, max }), i + 4))
        } else if shape.eq_ignore_ascii_case(b"EQ") {
            let v = IndexValue::parse_literal(spec_ty, args.get(i + 2).ok_or("ERR truncated view leaf")?)
                .ok_or("ERR leaf value does not coerce to the index type")?;
            Ok((Tree::Leaf(Leaf { index, min: v.clone(), max: v }), i + 3))
        } else {
            Err("ERR view leaf shape must be RANGE|EQ")
        }
    }
}

/// `VIEW.CREATE <name> QUERY <tree…> ORDER BY <index> [DESC]
/// [MODE virtual|materialized] [TOPK k] [VIA tpl]`.
pub(crate) fn cmd_view_create<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>, data_dir: Option<&Path>) {
    if args.len() < 8 || !args[2].eq_ignore_ascii_case(b"QUERY") {
        return encode_error(out, "ERR usage: VIEW.CREATE name QUERY <tree> ORDER BY idx [DESC] [MODE v|m] [TOPK k] [VIA tpl]");
    }
    let (tree, mut i) = match parse_tree(args, 3, 1) {
        Ok(t) => t,
        Err(e) => return encode_error(out, e),
    };
    if !(args.get(i).is_some_and(|t| t.eq_ignore_ascii_case(b"ORDER"))
        && args.get(i + 1).is_some_and(|t| t.eq_ignore_ascii_case(b"BY")))
    {
        return encode_error(out, "ERR ORDER BY <index> is required");
    }
    let Some(order_by) = args.get(i + 2).map(|t| t.to_vec()) else {
        return encode_error(out, "ERR ORDER BY <index> is required");
    };
    if index_runtime::catalog().and_then(|c| c.get(&order_by).map(|_| ())).is_none() {
        return encode_error(out, "ERR ORDER BY references unknown index");
    }
    i += 3;
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
                None => return encode_error(out, "ERR MODE requires virtual|materialized"),
            };
            mode = if m.eq_ignore_ascii_case(b"virtual") {
                ViewMode::Virtual
            } else if m.eq_ignore_ascii_case(b"materialized") {
                ViewMode::Materialized { top_k: 0 }
            } else {
                return encode_error(out, "ERR MODE must be virtual|materialized");
            };
            i += 2;
        } else if t.eq_ignore_ascii_case(b"TOPK") {
            top_k = match args.get(i + 1).and_then(|v| std::str::from_utf8(v).ok()).and_then(|s| s.parse().ok()) {
                Some(k) => k,
                None => return encode_error(out, "ERR TOPK must be an integer"),
            };
            i += 2;
        } else if t.eq_ignore_ascii_case(b"VIA") {
            via = args.get(i + 1).map(|v| v.to_vec());
            if via.is_none() {
                return encode_error(out, "ERR VIA requires a template");
            }
            i += 2;
        } else {
            return encode_error(out, "ERR syntax error");
        }
    }
    if let ViewMode::Materialized { .. } = mode {
        mode = ViewMode::Materialized { top_k };
    } else if top_k != 0 {
        return encode_error(out, "ERR TOPK requires MODE materialized");
    }
    let spec = ViewSpec { name: args[1].to_vec(), tree, order_by, desc, mode, via };
    let mut cat = view_runtime::catalog().map(|c| (*c).clone()).unwrap_or_default();
    match cat.create(spec) {
        Ok(()) => {
            persist_sidecar(data_dir, &cat);
            view_runtime::install_catalog(cat);
            out.extend_from_slice(b"+OK\r\n");
        }
        Err(e) => encode_error(out, e),
    }
}

/// `VIEW.DROP <name>`.
pub(crate) fn cmd_view_drop<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>, data_dir: Option<&Path>) {
    if args.len() != 2 {
        return encode_error(out, "ERR usage: VIEW.DROP name");
    }
    let mut cat = view_runtime::catalog().map(|c| (*c).clone()).unwrap_or_default();
    let hit = cat.drop_view(&args[1]);
    if hit {
        persist_sidecar(data_dir, &cat);
        view_runtime::install_catalog(cat);
    }
    encode_integer(out, i64::from(hit));
}

// ---------- extension fan-out ----------

/// Per-shard half for VIEW.QUERY / VIEW.LIST / VIEW.VERIFY /
/// VIEW.REBUILD / VIEW.EXPLAIN.
pub(crate) fn extension_op(store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let verb = argv.first().map(Vec::as_slice).unwrap_or(b"");
    if verb.eq_ignore_ascii_case(b"VIEW.QUERY") {
        return op_query(store, argv);
    }
    if verb.eq_ignore_ascii_case(b"VIEW.LIST") {
        return vec![ST_OK]; // catalog is global — the reduce renders it
    }
    if verb.eq_ignore_ascii_case(b"VIEW.VERIFY") {
        return op_stats(store, argv, verb);
    }
    if verb.eq_ignore_ascii_case(b"VIEW.REBUILD") {
        if let Some(name) = argv.get(1) {
            view_runtime::schedule_rebuild(name);
            view_runtime::on_tick(store); // run it now on this shard
        }
        return vec![ST_OK];
    }
    if verb.eq_ignore_ascii_case(b"VIEW.EXPLAIN") {
        return op_explain(store, argv);
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
    // (RFC: target missing = nil).
    let mut chunk = vec![ST_OK];
    let mut body = Vec::new();
    let mut hits = 0u32;
    for (row_idx, row) in rows.chunks(3).enumerate() {
        let [_member, _order, target] = row else { break };
        if store.exists(&[target.to_vec()]) == 0 {
            continue;
        }
        hits += 1;
        body.extend_from_slice(&(row_idx as u32).to_le_bytes());
        for f in fields {
            match store.hget(target, f) {
                Ok(Some(v)) => {
                    let v = v.to_vec();
                    body.extend_from_slice(&(v.len() as u32).to_le_bytes());
                    body.extend_from_slice(&v);
                }
                _ => body.extend_from_slice(&u32::MAX.to_le_bytes()),
            }
        }
    }
    chunk.extend_from_slice(&hits.to_le_bytes());
    chunk.extend_from_slice(&body);
    chunk
}

fn op_query(store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some(q) = QueryArgs::parse(argv) else {
        return vec![crate::cmd_index_query::ST_BADARGS];
    };
    match view_runtime::shard_page(store, &q.name, q.after.as_ref(), q.limit) {
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
        Err(e) if e.starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(_) => vec![ST_NOINDEX],
    }
}

fn op_stats(store: &mut Store, argv: &[Vec<u8>], _verb: &[u8]) -> Vec<u8> {
    let Some(name) = argv.get(1) else {
        return vec![crate::cmd_index_query::ST_BADARGS];
    };
    match view_runtime::shard_stats(store, name) {
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

fn op_explain(store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some(name) = argv.get(1) else {
        return vec![crate::cmd_index_query::ST_BADARGS];
    };
    let Some(spec) = view_runtime::catalog().and_then(|c| c.get(name).cloned()) else {
        return vec![ST_NOINDEX];
    };
    // Per-leaf local cardinalities.
    let counts = index_runtime::with_segment_resolver(store, |seg| {
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

/// Origin reduce for VIEW.* verbs.
pub(crate) fn extension_reduce(argv: &[Vec<u8>], chunks: Vec<Vec<u8>>) -> Vec<u8> {
    let verb = argv.first().map(Vec::as_slice).unwrap_or(b"");
    let mut out = Vec::new();
    for c in &chunks {
        match c.first().copied() {
            Some(x) if x == crate::cmd_index_query::ST_BADARGS => {
                encode_error(&mut out, "ERR bad VIEW arguments");
                return out;
            }
            Some(x) if x == ST_NOINDEX => {
                encode_error(&mut out, "ERR no such view");
                return out;
            }
            Some(x) if x == ST_BUILDING => {
                encode_error(&mut out, "INDEXBUILDING view's base index is still building");
                return out;
            }
            None => {
                encode_error(&mut out, "ERR bad VIEW arguments");
                return out;
            }
            _ => {}
        }
    }
    if verb.eq_ignore_ascii_case(b"VIEW.REBUILD") {
        out.extend_from_slice(b"+OK\r\n");
        return out;
    }
    if verb.eq_ignore_ascii_case(b"VIEW.HYDRATE") {
        return reduce_hydrate(argv, &chunks);
    }
    if verb.eq_ignore_ascii_case(b"VIEW.LIST") || verb.eq_ignore_ascii_case(b"VIEW.VERIFY") {
        return reduce_stats(argv, &chunks);
    }
    if verb.eq_ignore_ascii_case(b"VIEW.EXPLAIN") {
        return reduce_explain(argv, &chunks);
    }
    reduce_query(argv, chunks)
}

fn reduce_query(argv: &[Vec<u8>], chunks: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(q) = QueryArgs::parse(argv) else {
        encode_error(&mut out, "ERR bad VIEW arguments");
        return out;
    };
    let spec = view_runtime::catalog().and_then(|c| c.get(&q.name).cloned());
    let desc = spec.as_ref().is_some_and(|s| s.desc);
    let mut all: Vec<(IndexValue, Vec<u8>)> = Vec::new();
    for c in &chunks {
        let mut pos = 1usize;
        let Some(n) = crate::cmd_index_reduce::read_u32_at(c, &mut pos) else { continue };
        for _ in 0..n {
            let Some(key) = crate::cmd_index_reduce::read_kbytes_at(c, &mut pos) else { break };
            let Some(v) = crate::cmd_index_query::decode_value(c, &mut pos) else { break };
            pos += 1; // per-shard hydration count is always 0 here
            all.push((v, key));
        }
    }
    all.sort();
    if desc {
        all.reverse();
    }
    all.truncate(q.limit);
    let next = if all.len() == q.limit {
        all.last()
            .map(|(v, k)| crate::cmd_index_reduce::encode_view_cursor_bytes(v, k))
            .unwrap_or_else(|| b"0".to_vec())
    } else {
        b"0".to_vec()
    };
    // FIELDS + VIA: phase 2 — derive target keys via the template and
    // continue with an internal hydration fan-out (the continuation
    // argv carries the full phase-1 result, stateless).
    if !q.fields.is_empty() {
        let Some(via) = spec.as_ref().and_then(|s| s.via.clone()) else {
            encode_error(&mut out, "ERR FIELDS requires the view to declare VIA");
            return out;
        };
        let mut argv2: Vec<Vec<u8>> = vec![b"VIEW.HYDRATE".to_vec(), next.clone()];
        argv2.push((q.fields.len() as u32).to_le_bytes().to_vec());
        argv2.extend(q.fields.iter().cloned());
        for (v, k) in &all {
            argv2.push(k.clone());
            argv2.push(crate::cmd_index_reduce::value_repr_pub(v));
            argv2.push(expand_via(&via, k));
        }
        let mut cont = vec![0u8];
        cont.extend_from_slice(&(argv2.len() as u32).to_le_bytes());
        for item in &argv2 {
            cont.extend_from_slice(&(item.len() as u32).to_le_bytes());
            cont.extend_from_slice(item);
        }
        return cont;
    }
    encode_array_len(&mut out, 2);
    encode_bulk(&mut out, &next);
    encode_array_len(&mut out, (all.len() * 2) as i64);
    for (v, k) in &all {
        encode_bulk(&mut out, k);
        encode_bulk(&mut out, &crate::cmd_index_reduce::value_repr_pub(v));
    }
    out
}

/// Expand a VIA template: `{key}` = the member key, `{key.N}` = the
/// member key's N-th ':'-separated segment (missing segment = empty).
fn expand_via(tpl: &[u8], key: &[u8]) -> Vec<u8> {
    let t = String::from_utf8_lossy(tpl);
    let k = String::from_utf8_lossy(key);
    let mut out = String::with_capacity(t.len() + k.len());
    let mut rest = t.as_ref();
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let ph = &rest[start + 1..start + end];
        if ph == "key" {
            out.push_str(&k);
        } else if let Some(n) = ph.strip_prefix("key.").and_then(|s| s.parse::<usize>().ok()) {
            out.push_str(k.split(':').nth(n).unwrap_or(""));
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out.into_bytes()
}

fn reduce_stats(argv: &[Vec<u8>], chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut members, mut bytes, mut excluded, mut building) = (0u64, 0u64, 0u64, false);
    for c in chunks {
        building |= c.get(1).copied().unwrap_or(0) != 0;
        let mut pos = 2usize;
        for slot in 0..3 {
            let Some(w) = c.get(pos..pos + 8) else { break };
            let v = u64::from_le_bytes(w.try_into().expect("8 bytes"));
            match slot {
                0 => members += v,
                1 => bytes += v,
                _ => excluded += v,
            }
            pos += 8;
        }
    }
    let verb = argv.first().map(Vec::as_slice).unwrap_or(b"");
    if verb.eq_ignore_ascii_case(b"VIEW.LIST") {
        // One row per declared view; stats only for the requested…
        // LIST takes no name: emit catalog + the queried view's stats
        // aggregated per-view is a second fanout — v1 reports specs +
        // this fanout's per-view stats only when a name is passed.
        let cat = view_runtime::catalog();
        let n = cat.as_ref().map_or(0, |c| c.len());
        encode_array_len(&mut out, n as i64);
        if let Some(cat) = cat {
            for spec in cat.iter() {
                encode_array_len(&mut out, 8);
                encode_bulk(&mut out, b"name");
                encode_bulk(&mut out, &spec.name);
                encode_bulk(&mut out, b"mode");
                encode_bulk(&mut out, match spec.mode {
                    ViewMode::Virtual => b"virtual" as &[u8],
                    ViewMode::Materialized { .. } => b"materialized",
                });
                encode_bulk(&mut out, b"order_by");
                encode_bulk(&mut out, &spec.order_by);
                encode_bulk(&mut out, b"leaves");
                encode_bulk(&mut out, spec.tree.leaves().to_string().as_bytes());
            }
        }
        return out;
    }
    encode_array_len(&mut out, 8);
    encode_bulk(&mut out, b"members");
    encode_bulk(&mut out, members.to_string().as_bytes());
    encode_bulk(&mut out, b"bytes");
    encode_bulk(&mut out, bytes.to_string().as_bytes());
    encode_bulk(&mut out, b"order_excluded");
    encode_bulk(&mut out, excluded.to_string().as_bytes());
    encode_bulk(&mut out, b"rebuilding");
    encode_bulk(&mut out, if building { b"1" } else { b"0" });
    out
}

fn reduce_explain(argv: &[Vec<u8>], chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(spec) = argv
        .get(1)
        .and_then(|n| view_runtime::catalog().and_then(|c| c.get(n).cloned()))
    else {
        encode_error(&mut out, "ERR no such view");
        return out;
    };
    // Sum per-leaf counts across shards.
    let nleaves = spec.tree.leaves();
    let mut counts = vec![0u64; nleaves];
    for c in chunks {
        let n = c.get(1).copied().unwrap_or(0) as usize;
        let mut pos = 2usize;
        for cnt in counts.iter_mut().take(n.min(nleaves)) {
            if let Some(w) = c.get(pos..pos + 8) {
                *cnt += u64::from_le_bytes(w.try_into().expect("8 bytes"));
            }
            pos += 8;
        }
    }
    let mut tree_txt = String::new();
    render_tree(&spec.tree, &mut tree_txt);
    encode_array_len(&mut out, 4);
    encode_bulk(&mut out, b"tree");
    encode_bulk(&mut out, tree_txt.as_bytes());
    encode_bulk(&mut out, b"leaf_counts");
    encode_bulk(
        &mut out,
        counts
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(",")
            .as_bytes(),
    );
    out
}

fn render_tree(t: &Tree, out: &mut String) {
    match t {
        Tree::Leaf(l) => out.push_str(&format!("{}[..]", String::from_utf8_lossy(&l.index))),
        Tree::And(a, b) | Tree::Or(a, b) | Tree::Diff(a, b) => {
            let op = match t {
                Tree::And(..) => "AND",
                Tree::Or(..) => "OR",
                _ => "DIFF",
            };
            out.push('(');
            out.push_str(op);
            out.push(' ');
            render_tree(a, out);
            out.push(' ');
            render_tree(b, out);
            out.push(')');
        }
    }
}

/// Phase-2 reduce: argv carries the phase-1 result (cursor + fields +
/// (member, order, target) rows); chunks carry per-target field
/// values from the owning shards. Reply = the final hydrated rows.
fn reduce_hydrate(argv: &[Vec<u8>], chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let cursor = argv.get(1).cloned().unwrap_or_else(|| b"0".to_vec());
    let nf = argv
        .get(2)
        .and_then(|b| b.get(..4))
        .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")) as usize)
        .unwrap_or(0);
    let fields: Vec<&[u8]> = argv[3..3 + nf].iter().map(Vec::as_slice).collect();
    let rows: Vec<&[Vec<u8>]> = argv[3 + nf..].chunks(3).collect();
    // row_idx → field values
    let mut hydrated: Vec<Option<Vec<Option<Vec<u8>>>>> = vec![None; rows.len()];
    for c in chunks {
        let Some(hits) = c.get(1..5).map(|b| u32::from_le_bytes(b.try_into().expect("4"))) else {
            continue;
        };
        let mut pos = 5usize;
        for _ in 0..hits {
            let Some(idx) = c.get(pos..pos + 4).map(|b| u32::from_le_bytes(b.try_into().expect("4")) as usize) else { break };
            pos += 4;
            let mut vals = Vec::with_capacity(nf);
            for _ in 0..nf {
                let Some(len) = c.get(pos..pos + 4).map(|b| u32::from_le_bytes(b.try_into().expect("4"))) else { break };
                pos += 4;
                if len == u32::MAX {
                    vals.push(None);
                } else {
                    let Some(v) = c.get(pos..pos + len as usize) else { break };
                    vals.push(Some(v.to_vec()));
                    pos += len as usize;
                }
            }
            if idx < hydrated.len() {
                hydrated[idx] = Some(vals);
            }
        }
    }
    encode_array_len(&mut out, 2);
    encode_bulk(&mut out, &cursor);
    encode_array_len(&mut out, rows.len() as i64);
    for (i, row) in rows.iter().enumerate() {
        let (member, order) = (&row[0], &row[1]);
        encode_array_len(&mut out, (2 + fields.len() * 2) as i64);
        encode_bulk(&mut out, member);
        encode_bulk(&mut out, order);
        let vals = hydrated[i].clone().unwrap_or_else(|| vec![None; nf]);
        for (f, v) in fields.iter().zip(vals.iter().chain(std::iter::repeat(&None))) {
            encode_bulk(&mut out, f);
            match v {
                Some(b) => encode_bulk(&mut out, b),
                None => out.extend_from_slice(b"$-1\r\n"),
            }
        }
    }
    out
}

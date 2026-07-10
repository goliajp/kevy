//! v2.6 — VIEW.* origin-side reduce: merge per-shard chunks into RESP
//! (split from [`crate::cmd_view`] under the 500-LOC house rule).

use kevy_index::{IndexValue, Tree, ViewMode};
use kevy_resp::{encode_array_len, encode_bulk, encode_error};

use crate::cmd_index_query::{ST_BUILDING, ST_NOINDEX};
use crate::cmd_view::QueryArgs;
use crate::view_runtime;

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
        return hydrate_continuation(&q.fields, &via, &next, &all);
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

/// Build the stateless VIEW.HYDRATE continuation (kevy-rt convention:
/// a reduce reply starting with NUL re-fans the embedded argv).
fn hydrate_continuation(
    fields: &[Vec<u8>],
    via: &[u8],
    next: &[u8],
    all: &[(IndexValue, Vec<u8>)],
) -> Vec<u8> {
    let mut argv2: Vec<Vec<u8>> = vec![b"VIEW.HYDRATE".to_vec(), next.to_vec()];
    argv2.push((fields.len() as u32).to_le_bytes().to_vec());
    argv2.extend(fields.iter().cloned());
    for (v, k) in all {
        argv2.push(k.clone());
        argv2.push(crate::cmd_index_reduce::value_repr_pub(v));
        argv2.push(expand_via(via, k));
    }
    let mut cont = vec![0u8];
    cont.extend_from_slice(&(argv2.len() as u32).to_le_bytes());
    for item in &argv2 {
        cont.extend_from_slice(&(item.len() as u32).to_le_bytes());
        cont.extend_from_slice(item);
    }
    cont
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
        return render_view_list();
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

/// One row per declared view; stats only for the requested…
/// LIST takes no name: emit catalog + the queried view's stats
/// aggregated per-view is a second fanout — v1 reports specs +
/// this fanout's per-view stats only when a name is passed.
fn render_view_list() -> Vec<u8> {
    let mut out = Vec::new();
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
    let hydrated = decode_hydrated_rows(chunks, rows.len(), nf);
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

/// Decode the per-shard hydration chunks into a row_idx → field
/// values table (`None` = no shard owned that row's target).
fn decode_hydrated_rows(
    chunks: &[Vec<u8>],
    nrows: usize,
    nf: usize,
) -> Vec<Option<Vec<Option<Vec<u8>>>>> {
    let mut hydrated: Vec<Option<Vec<Option<Vec<u8>>>>> = vec![None; nrows];
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
    hydrated
}

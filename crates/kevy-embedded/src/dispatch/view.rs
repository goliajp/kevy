//! VIEW.* verbs: CREATE (argv tree grammar → typed `ViewTree`) /
//! DROP / LIST / QUERY. Reply shapes mirror the server's
//! `cmd_view_reduce`; `VIA` (wire-side hydration templating) is not
//! part of the embedded surface — in-process callers read fields
//! directly — so declaring it is rejected loudly rather than dropped.

use crate::store::Store;

use kevy_index::{IndexValue, Leaf, Tree, ViewMode};

use super::idx::{decode_cursor, encode_cursor, spec_of, value_repr};
use super::util::{arr, bulk, err, int, kevy_err, verb_name, wrong_args};

/// One VIEW request; `false` = verb not in this group.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    match up {
        b"VIEW.CREATE" => cmd_view_create(s, argv, out),
        b"VIEW.DROP" => {
            if argv.len() != 2 {
                err(out, "ERR usage: VIEW.DROP name");
            } else {
                int(out, i64::from(s.view_drop(&argv[1])));
            }
        }
        b"VIEW.LIST" => cmd_view_list(s, out),
        b"VIEW.QUERY" => cmd_view_query(s, argv, out),
        _ => return false,
    }
    true
}

/// One leaf of a view tree: an index name, a shape, and the literals the
/// shape needs — `RANGE min max` or `EQ value`.
///
/// Split from `parse_tree` for the 50-line rule, at the boundary the function
/// already had: everything above parses a node and recurses, everything
/// here parses a leaf and does not.
fn parse_leaf(s: &Store, argv: &[Vec<u8>], i: usize) -> Result<(Tree, usize), &'static str> {
    let tok = argv.get(i).ok_or("ERR truncated view tree")?;
    let index = tok.to_vec();
    let spec_ty =
        spec_of(s, &index).map(|sp| sp.ty).ok_or("ERR view leaf references unknown index")?;
    let shape = argv.get(i + 1).ok_or("ERR truncated view leaf")?;
    if shape.eq_ignore_ascii_case(b"RANGE") {
        let min =
            IndexValue::parse_literal(spec_ty, argv.get(i + 2).ok_or("ERR truncated view leaf")?)
                .ok_or("ERR leaf min does not coerce to the index type")?;
        let max =
            IndexValue::parse_literal(spec_ty, argv.get(i + 3).ok_or("ERR truncated view leaf")?)
                .ok_or("ERR leaf max does not coerce to the index type")?;
        Ok((Tree::Leaf(Leaf { index, min, max }), i + 4))
    } else if shape.eq_ignore_ascii_case(b"EQ") {
        let v =
            IndexValue::parse_literal(spec_ty, argv.get(i + 2).ok_or("ERR truncated view leaf")?)
                .ok_or("ERR leaf value does not coerce to the index type")?;
        Ok((Tree::Leaf(Leaf { index, min: v.clone(), max: v }), i + 3))
    } else {
        Err("ERR view leaf shape must be RANGE|EQ")
    }
}

/// Parse one tree node starting at `i`; parens are separate argv
/// tokens: `( AND|OR|DIFF <sub> <sub> )` | `<index> RANGE min max` |
/// `<index> EQ v` (server `cmd_view::parse_tree`).
fn parse_tree(
    s: &Store,
    argv: &[Vec<u8>],
    i: usize,
    depth: usize,
) -> Result<(Tree, usize), &'static str> {
    if depth > kevy_index::MAX_TREE_DEPTH {
        return Err("ERR view tree deeper than 3");
    }
    let tok = argv.get(i).ok_or("ERR truncated view tree")?;
    if tok.as_slice() == b"(" {
        let op = argv.get(i + 1).ok_or("ERR truncated view tree")?;
        let (a, ni) = parse_tree(s, argv, i + 2, depth + 1)?;
        let (b, ni) = parse_tree(s, argv, ni, depth + 1)?;
        if argv.get(ni).map(Vec::as_slice) != Some(b")") {
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
        parse_leaf(s, argv, i)
    }
}

/// `VIEW.CREATE name QUERY <tree…> ORDER BY idx [DESC] [MODE v|m]
/// [TOPK k]`.
fn cmd_view_create(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if argv.len() < 8 || !argv[2].eq_ignore_ascii_case(b"QUERY") {
        return err(
            out,
            "ERR usage: VIEW.CREATE name QUERY <tree> ORDER BY idx [DESC] [MODE v|m] [TOPK k] [VIA tpl]",
        );
    }
    let (tree, mut i) = match parse_tree(s, argv, 3, 1) {
        Ok(t) => t,
        Err(e) => return err(out, e),
    };
    if !(argv.get(i).is_some_and(|t| t.eq_ignore_ascii_case(b"ORDER"))
        && argv.get(i + 1).is_some_and(|t| t.eq_ignore_ascii_case(b"BY")))
    {
        return err(out, "ERR ORDER BY <index> is required");
    }
    let Some(order_by) = argv.get(i + 2).cloned() else {
        return err(out, "ERR ORDER BY <index> is required");
    };
    if spec_of(s, &order_by).is_none() {
        return err(out, "ERR ORDER BY references unknown index");
    }
    i += 3;
    let (desc, mode, top_k) = match parse_create_opts(argv, i) {
        Ok(t) => t,
        Err(e) => return err(out, e),
    };
    let mode = match mode {
        ViewMode::Materialized { .. } => ViewMode::Materialized { top_k },
        ViewMode::Virtual if top_k != 0 => {
            return err(out, "ERR TOPK requires MODE materialized");
        }
        v => v,
    };
    match s.view_create(&argv[1], tree, &order_by, desc, mode) {
        Ok(()) => out.extend_from_slice(b"+OK\r\n"),
        Err(e) => kevy_err(out, &e),
    }
}

/// `[DESC] [MODE virtual|materialized] [TOPK k]` tail; `VIA` is
/// rejected (embedded views carry no hydration template).
fn parse_create_opts(
    argv: &[Vec<u8>],
    mut i: usize,
) -> Result<(bool, ViewMode, u32), &'static str> {
    let mut desc = false;
    let mut mode = ViewMode::Virtual;
    let mut top_k = 0u32;
    while i < argv.len() {
        let t = &argv[i];
        if t.eq_ignore_ascii_case(b"DESC") {
            desc = true;
            i += 1;
        } else if t.eq_ignore_ascii_case(b"MODE") {
            let Some(m) = argv.get(i + 1) else {
                return Err("ERR MODE requires virtual|materialized");
            };
            mode = if m.eq_ignore_ascii_case(b"virtual") {
                ViewMode::Virtual
            } else if m.eq_ignore_ascii_case(b"materialized") {
                ViewMode::Materialized { top_k: 0 }
            } else {
                return Err("ERR MODE must be virtual|materialized");
            };
            i += 2;
        } else if t.eq_ignore_ascii_case(b"TOPK") {
            top_k = argv
                .get(i + 1)
                .and_then(|v| std::str::from_utf8(v).ok())
                .and_then(|s| s.parse().ok())
                .ok_or("ERR TOPK must be an integer")?;
            i += 2;
        } else if t.eq_ignore_ascii_case(b"VIA") {
            return Err("ERR VIA is not supported by the embedded engine (read fields in-process)");
        } else {
            return Err("ERR syntax error");
        }
    }
    Ok((desc, mode, top_k))
}

/// `VIEW.LIST` — 8-field rows matching the server's `render_view_list`.
fn cmd_view_list(s: &Store, out: &mut Vec<u8>) {
    let g = s.views.catalog.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    let specs: Vec<_> = g.1.iter().cloned().collect();
    drop(g);
    arr(out, specs.len());
    for spec in &specs {
        arr(out, 8);
        bulk(out, b"name");
        bulk(out, &spec.name);
        bulk(out, b"mode");
        bulk(
            out,
            match spec.mode {
                ViewMode::Virtual => b"virtual" as &[u8],
                ViewMode::Materialized { .. } => b"materialized",
            },
        );
        bulk(out, b"order_by");
        bulk(out, &spec.order_by);
        bulk(out, b"leaves");
        bulk(out, spec.tree.leaves().to_string().as_bytes());
    }
}

/// `VIEW.QUERY name [LIMIT n] [CURSOR c]` — `[cursor, [member,
/// order, …]]`; `FIELDS` requires VIA, which embedded views never
/// declare, so it answers the server's exact requires-VIA error.
fn cmd_view_query(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let Some(name) = argv.get(1) else {
        // Too few arguments, not wrong ones: the server names the verb here
        // and so does this. `BAD` below stays for arguments that are
        // present and will not parse.
        return wrong_args(out, &verb_name(argv));
    };
    let (limit, after) = match parse_query_opts(argv) {
        Ok(t) => t,
        Err(e) => return err(out, e),
    };
    match s.view_query(name, after.as_ref(), limit) {
        Err(crate::KevyError::NotFound(_)) => err(out, "ERR no such view"),
        Err(e) => kevy_err(out, &e),
        Ok((rows, next)) => {
            arr(out, 2);
            match next {
                Some((v, k)) => bulk(out, &encode_cursor(&v, &k)),
                None => bulk(out, b"0"),
            }
            arr(out, rows.len() * 2);
            for (k, v) in &rows {
                bulk(out, k);
                bulk(out, &value_repr(v));
            }
        }
    }
}

/// `[LIMIT n] [CURSOR c]` — the VIEW.QUERY option tail; `FIELDS`
/// answers the server's requires-VIA error.
type QueryOpts = (usize, Option<(IndexValue, Vec<u8>)>);

fn parse_query_opts(argv: &[Vec<u8>]) -> Result<QueryOpts, &'static str> {
    const BAD: &str = "ERR bad VIEW arguments";
    let mut limit = 100usize;
    let mut after: Option<(IndexValue, Vec<u8>)> = None;
    let mut i = 2;
    while i < argv.len() {
        let t = &argv[i];
        if t.eq_ignore_ascii_case(b"LIMIT") {
            limit = argv
                .get(i + 1)
                .and_then(|v| std::str::from_utf8(v).ok())
                .and_then(|v| v.parse().ok())
                .ok_or(BAD)?;
            i += 2;
        } else if t.eq_ignore_ascii_case(b"CURSOR") {
            let raw = argv.get(i + 1).ok_or(BAD)?;
            if raw.as_slice() != b"0" {
                after = Some(decode_cursor(raw).ok_or(BAD)?);
            }
            i += 2;
        } else if t.eq_ignore_ascii_case(b"FIELDS") {
            return Err("ERR FIELDS requires the view to declare VIA");
        } else {
            return Err(BAD);
        }
    }
    Ok((limit.clamp(1, 10_000), after))
}

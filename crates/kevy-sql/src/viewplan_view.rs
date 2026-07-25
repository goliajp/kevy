//! The engine-view emission half of pass 2 (split from `viewplan.rs`
//! for the 500-LOC house rule): constant-predicate views whose columns
//! each carry their own single-column index become a `VIEW.CREATE`
//! with a balanced `( AND … )` tree.

use crate::ast::CreateView;
use crate::schema::Table;
use crate::viewplan_norm::{BV, ColPred, Shape};

/// `Some(argv)` when every predicate column carries its own usable
/// single-column index (≤ 4 leaves) and the ORDER BY column (if any)
/// does too.
pub(crate) fn try_engine_view(
    v: &CreateView,
    t: &Table,
    preds: &[ColPred],
    order: Option<&(String, bool)>,
) -> Option<Vec<String>> {
    if preds.len() > 4 {
        return None;
    }
    for p in preds {
        let ix = t.index_on(&p.col)?;
        if matches!(p.shape, Shape::Range(..)) && ix.unique {
            return None; // a Unique path serves points; ranges need Range.
        }
    }
    let (order_col, desc) = match order {
        Some((c, d)) => {
            t.index_on(c)?;
            (c.clone(), *d)
        }
        None => (preds[0].col.clone(), false),
    };
    let leaves: Vec<Vec<String>> = preds.iter().map(|p| leaf_tokens(t, p)).collect();
    let mut argv = vec!["VIEW.CREATE".to_string(), v.name.clone(), "QUERY".to_string()];
    argv.extend(tree_tokens(&leaves));
    argv.push("ORDER".into());
    argv.push("BY".into());
    argv.push(format!("{}.{order_col}", t.name));
    if desc {
        argv.push("DESC".into());
    }
    Some(argv)
}

fn leaf_tokens(t: &Table, p: &ColPred) -> Vec<String> {
    let name = format!("{}.{}", t.name, p.col);
    match &p.shape {
        Shape::Eq(BV::Lit(x)) => vec![name, "EQ".into(), x.clone()],
        Shape::Range(BV::Lit(lo), BV::Lit(hi)) => {
            vec![name, "RANGE".into(), lo.clone(), hi.clone()]
        }
        // Callers guarantee constant bounds before building a view.
        _ => unreachable!("engine views are constant-bound by construction"),
    }
}

/// Balanced `( AND … )` nesting: ≤ 4 leaves fits the engine's
/// depth-3 / 4-leaf view-tree budget.
fn tree_tokens(leaves: &[Vec<String>]) -> Vec<String> {
    if leaves.len() == 1 {
        return leaves[0].clone();
    }
    let (a, b) = leaves.split_at(leaves.len() / 2);
    let mut out = vec!["(".to_string(), "AND".to_string()];
    out.extend(tree_tokens(a));
    out.extend(tree_tokens(b));
    out.push(")".into());
    out
}

pub(crate) fn view_read_note(v: &CreateView, fields: &[String]) -> String {
    // VIEW.QUERY pages keys (its FIELDS clause is VIA-hydration only);
    // the selected columns hydrate per row.
    let limit = v.limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
    format!(
        "view {}: read with VIEW.QUERY {}{limit}, then hydrate rows with HMGET <key> {}",
        v.name,
        v.name,
        fields.join(" ")
    )
}

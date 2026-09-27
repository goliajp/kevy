//! Where a reshard takes its slots from: `--cluster-from`, or asked.

use super::log::{self, Level};
use super::reshard::{master_by_id, not_a_master};
use super::topology::Cluster;
use crate::rcli::send::write_out;

/// The source masters, in the order given; `None` after saying why.
pub(crate) fn sources(c: &Cluster, target: usize) -> Option<Vec<usize>> {
    let chosen = match &c.cfg.from {
        Some(list) => from_flag(c, target, list)?,
        None => asked(c, target)?,
    };
    if chosen.is_empty() {
        crate::rcli::session::eprint_bytes(&[b"*** No source nodes given, operation aborted.\n"]);
        return None;
    }
    Some(chosen)
}

/// Every master but the target, in table order.
fn all_but(c: &Cluster, target: usize) -> Vec<usize> {
    (0..c.nodes.len()).filter(|&i| i != target && c.nodes[i].is_master()).collect()
}

fn from_flag(c: &Cluster, target: usize, list: &[u8]) -> Option<Vec<usize>> {
    if list == b"all" {
        return Some(all_but(c, target));
    }
    let mut chosen = Vec::new();
    for id in list.split(|&b| b == b',') {
        let Some(i) = master_by_id(c, id) else {
            not_a_master(c, id);
            return None;
        };
        if !add(c, &mut chosen, target, i) {
            continue;
        }
    }
    Some(chosen)
}

fn asked(c: &Cluster, target: usize) -> Option<Vec<usize>> {
    write_out(b"Please enter all the source node IDs.\n  Type 'all' to use all the nodes as source nodes for the hash slots.\n  Type 'done' once you entered all the source nodes IDs.\n");
    let mut chosen = Vec::new();
    loop {
        let prompt = format!("Source node #{}: ", chosen.len() + 1);
        let line = super::ask::line(prompt.as_bytes())?;
        match line.as_slice() {
            b"done" => return Some(chosen),
            b"all" => return Some(all_but(c, target)),
            id => match master_by_id(c, id) {
                Some(i) => {
                    add(c, &mut chosen, target, i);
                }
                None => {
                    not_a_master(c, id);
                    return None;
                }
            },
        }
    }
}

/// Add a source unless it is the target; `false` when refused.
fn add(c: &Cluster, chosen: &mut Vec<usize>, target: usize, i: usize) -> bool {
    if i == target {
        log::line(
            c.cfg.color,
            Level::Err,
            b"*** It is not possible to use the target node as source node.",
        );
        return false;
    }
    if !chosen.contains(&i) {
        chosen.push(i);
    }
    true
}

//! `--cluster del-node host:port node_id`: move an empty node's replicas to
//! other masters, have every node forget it, and reset it.

use super::addr;
use super::config::Config;
use super::log::{self, Level};
use super::topology::Cluster;
use crate::rcli::opts::Opts;

/// Run `del-node`; the exit code.
pub(crate) fn run(opts: &Opts, cfg: Config, args: &[Vec<u8>]) -> u8 {
    let (Some(entry), Some(id)) =
        (args.first().and_then(|a| addr::entry(std::slice::from_ref(a))), args.get(1))
    else {
        return addr::report_invalid();
    };
    let text = [&b">>> Removing node "[..], id, b" from cluster ", &entry.shown()].concat();
    log::line(cfg.color, Level::Info, &text);
    let Some(mut c) = Cluster::load(opts, cfg, &entry) else { return 1 };
    let color = c.cfg.color;
    let Some(gone) = c.nodes.iter().position(|n| n.rec.id == *id) else {
        log::line(color, Level::Err, &[&b"[ERR] No such node ID "[..], id].concat());
        return 1;
    };
    // What the node itself says: the others learn of slots given up only
    // when someone claims them.
    if !c.nodes[gone].own_slots.is_empty() {
        let text = [
            &b"[ERR] Node "[..],
            &c.nodes[gone].shown(),
            b" is not empty! Reshard data away and try again.",
        ]
        .concat();
        log::line(color, Level::Err, &text);
        return 1;
    }
    log::line(color, Level::Info, b">>> Sending CLUSTER FORGET messages to the cluster...");
    for i in 0..c.nodes.len() {
        if i == gone {
            continue;
        }
        if c.nodes[i].rec.master.as_deref() == Some(id.as_slice()) {
            adopt(&mut c, i, gone);
        }
        let _ = c.nodes[i].link.request(&[b"CLUSTER", b"FORGET", id]); // a node that already forgot it says so
    }
    log::line(color, Level::Info, b">>> Sending CLUSTER RESET SOFT to the deleted node.");
    let _ = c.nodes[gone].link.request(&[b"CLUSTER", b"RESET", b"SOFT"]); // the node is out of the cluster either way
    0
}

/// Make replica `i` follow the master with fewest replicas.
fn adopt(c: &mut Cluster, i: usize, gone: usize) {
    let masters = (0..c.nodes.len()).filter(|&m| m != gone && c.nodes[m].is_master());
    let Some(m) = masters.min_by_key(|&m| c.nodes[m].replicas) else { return };
    let text =
        [&b">>> "[..], &c.nodes[i].shown(), b" as replica of ", &c.nodes[m].shown()].concat();
    log::line(c.cfg.color, Level::Info, &text);
    let master_id = c.nodes[m].rec.id.clone();
    let _ = c.nodes[i].link.request(&[b"CLUSTER", b"REPLICATE", &master_id]); // the FORGET that follows still removes the node
    c.nodes[m].replicas += 1;
    c.nodes[i].rec.master = Some(master_id);
}

//! How nodes and the cluster's totals are printed.

use super::log::{self, Level};
use super::slots::bracketed;
use super::topology::{Cluster, Node};
use kevy_resp::Reply;

/// `M: <id> host:port`, its slots, and its replicas or its master.
pub(crate) fn node(n: &Node) {
    let role: &[u8] = if n.is_master() { b"M" } else { b"S" };
    log::plain(&[role, b": ", &n.rec.id, b" ", &n.shown()].concat());
    let kind: &[u8] = if n.is_master() { b"master" } else { b"slave" };
    let count = format!(" ({} slots) ", n.rec.slots.count());
    let ranges = bracketed(&n.rec.slots.ranges());
    log::plain(&[&b"   slots:"[..], &ranges, count.as_bytes(), kind].concat());
    if let Some(master) = &n.rec.master {
        log::plain(&[b"   replicates ", master.as_slice()].concat());
    } else if n.replicas > 0 {
        log::plain(format!("   {} additional replica(s)", n.replicas).as_bytes());
    }
}

/// One line per master with its keys, slots and replicas, then the totals.
pub(crate) fn info(c: &mut Cluster) {
    let (mut keys, mut masters) = (0i64, 0usize);
    for n in c.nodes.iter_mut().filter(|n| n.is_master()) {
        let dbsize = match n.link.request(&[b"DBSIZE"]) {
            Ok(Reply::Int(k)) => k,
            _ => 0,
        };
        let id = &n.rec.id[..n.rec.id.len().min(8)];
        let tail = format!(
            "...) -> {dbsize} keys | {} slots | {} slaves.",
            n.rec.slots.count(),
            n.replicas
        );
        log::plain(&[&n.shown()[..], b" (", id, tail.as_bytes()].concat());
        keys += dbsize;
        masters += 1;
    }
    let color = c.cfg.color;
    log::line(color, Level::Ok, format!("[OK] {keys} keys in {masters} masters.").as_bytes());
    let per_slot = keys as f64 / super::slots::SLOTS as f64;
    log::plain(format!("{per_slot:.2} keys per slot on average.").as_bytes());
}

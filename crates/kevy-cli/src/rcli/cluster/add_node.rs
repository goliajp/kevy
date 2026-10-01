//! `--cluster add-node new existing`: join an empty node to a cluster, as a
//! master (with the cluster's functions) or as a replica.

use super::addr::{self, Addr};
use super::config::Config;
use super::log::{self, Level};
use super::topology::Cluster;
use crate::rcli::conn::Conn;
use crate::rcli::opts::Opts;
use crate::rcli::send::write_out;
use kevy_resp::Reply;

/// Run `add-node`; the exit code.
pub(crate) fn run(opts: &Opts, cfg: Config, args: &[Vec<u8>]) -> u8 {
    let (Some(new), Some(existing)) = (
        args.first().and_then(|a| addr::entry(std::slice::from_ref(a))),
        args.get(1).and_then(|a| addr::entry(std::slice::from_ref(a))),
    ) else {
        return addr::report_invalid();
    };
    let text =
        [&b">>> Adding node "[..], &new.shown(), b" to cluster ", &existing.shown()].concat();
    log::line(cfg.color, Level::Info, &text);
    let Some(mut c) = Cluster::load(opts, cfg, &existing) else { return 1 };
    if !super::check::run(&mut c) {
        return 1;
    }
    let master = if c.cfg.replica {
        match pick_master(&c) {
            Some(i) => Some(i),
            None => return 1,
        }
    } else {
        None
    };
    let Some(mut link) = open_new(&c, &new) else { return 1 };
    if master.is_none() && !copy_functions(&mut c, &mut link, &new) {
        return 1;
    }
    join(&mut c, &mut link, &new, master)
}

/// `--cluster-master-id`, or the master with fewest replicas.
fn pick_master(c: &Cluster) -> Option<usize> {
    let Some(id) = &c.cfg.master_id else {
        let least = c.nodes.iter().filter(|n| n.is_master()).map(|n| n.replicas).min()?;
        let i = c.nodes.iter().position(|n| n.is_master() && n.replicas == least)?;
        let text = [&b"Automatically selected master "[..], &c.nodes[i].shown()].concat();
        log::plain(&text);
        return Some(i);
    };
    let found = c.nodes.iter().position(|n| n.is_master() && n.rec.id.eq_ignore_ascii_case(id));
    if found.is_none() {
        log::line(c.cfg.color, Level::Err, &[&b"[ERR] No such master ID "[..], id].concat());
    }
    found
}

/// Connect to the new node and require it to be an empty cluster node.
fn open_new(c: &Cluster, new: &Addr) -> Option<Conn> {
    let Some(mut link) = super::link::open(&c.opts, new) else {
        let text = [&b"[ERR] Sorry, can't connect to node "[..], &new.shown()].concat();
        log::line(c.cfg.color, Level::Err, &text);
        return None;
    };
    super::create::vet(&c.cfg, &mut link, new)?;
    Some(link)
}

/// The cluster's functions onto a new master, which must have none.
fn copy_functions(c: &mut Cluster, link: &mut Conn, new: &Addr) -> bool {
    let color = c.cfg.color;
    log::line(color, Level::Info, b">>> Getting functions from cluster");
    let dump = c.nodes.first_mut().map(|n| n.link.request(&[b"FUNCTION", b"DUMP"]));
    let text = [
        &b">>> Send FUNCTION LIST to "[..],
        &new.shown(),
        b" to verify there is no functions in it",
    ]
    .concat();
    log::line(color, Level::Info, &text);
    if matches!(link.request(&[b"FUNCTION", b"LIST"]), Ok(Reply::Array(list)) if !list.is_empty()) {
        log::line(color, Level::Err, b">>> New node already contains functions and can not be added to the cluster. Use FUNCTION FLUSH and try again.");
        return false;
    }
    log::line(color, Level::Info, &[&b">>> Send FUNCTION RESTORE to "[..], &new.shown()].concat());
    if let Some(Ok(Reply::Bulk(payload))) = dump {
        // A restore error leaves the node without functions, which the
        // cluster tolerates; the node is still added.
        let _ = link.request(&[b"FUNCTION", b"RESTORE", &payload]);
    }
    true
}

fn join(c: &mut Cluster, link: &mut Conn, new: &Addr, master: Option<usize>) -> u8 {
    let color = c.cfg.color;
    let text =
        [&b">>> Send CLUSTER MEET to node "[..], &new.shown(), b" to make it join the cluster."]
            .concat();
    log::line(color, Level::Info, &text);
    let Some(entry) = c.nodes.first() else { return 1 };
    let Some(ip) = super::join::resolve(&entry.rec.host, entry.rec.port) else { return 1 };
    let port = entry.rec.port.to_string();
    if let Ok(Reply::Error(e)) =
        link.request(&[b"CLUSTER", b"MEET", ip.as_bytes(), port.as_bytes()])
    {
        log::line(color, Level::Err, &[&b"[ERR] "[..], &e].concat());
        return 1;
    }
    if let Some(m) = master {
        let master_id = c.nodes[m].rec.id.clone();
        write_out(b"Waiting for the cluster to join\n");
        super::join::wait_known(link, &master_id);
        write_out(b"\n");
        let text = [&b">>> Configure node as replica of "[..], &c.nodes[m].shown(), b"."].concat();
        log::line(color, Level::Info, &text);
        if let Ok(Reply::Error(e)) = link.request(&[b"CLUSTER", b"REPLICATE", &master_id]) {
            log::line(color, Level::Err, &[&b"[ERR] "[..], &e].concat());
            return 1;
        }
    }
    log::line(color, Level::Ok, b"[OK] New node added correctly.");
    0
}

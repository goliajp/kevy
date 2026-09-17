//! The cluster as seen from one node: every node it knows that answers,
//! each with an open connection.

use super::addr::Addr;
use super::config::Config;
use super::link;
use super::nodes_text::{self, Record};
use crate::rcli::conn::Conn;
use crate::rcli::opts::Opts;
use crate::rcli::session::eprint_bytes;

/// One reachable node.
pub(crate) struct Node {
    /// As the entry node sees it; its open slots as it sees itself.
    pub(crate) rec: Record,
    /// Loaded replicas of this node.
    pub(crate) replicas: usize,
    /// Who owns which slots, in this node's own view.
    pub(crate) signature: Vec<u8>,
    pub(crate) link: Conn,
}

impl Node {
    /// `host:port`.
    pub(crate) fn shown(&self) -> Vec<u8> {
        Addr { host: self.rec.host.clone(), port: self.rec.port }.shown()
    }

    pub(crate) fn is_master(&self) -> bool {
        self.rec.flags.master
    }
}

/// Every node loaded, the entry node first.
pub(crate) struct Cluster {
    pub(crate) nodes: Vec<Node>,
    /// Masters that could not be reached.
    pub(crate) unreachable_masters: usize,
    pub(crate) cfg: Config,
    pub(crate) opts: Opts,
}

impl Cluster {
    /// Load from `entry`; `None` after printing why when that cannot be done.
    pub(crate) fn load(opts: &Opts, cfg: Config, entry: &Addr) -> Option<Cluster> {
        let mut link = link::open(opts, entry)?;
        let (records, signature) = nodes_of(&mut link, entry, &cfg)?;
        let mut cluster =
            Cluster { nodes: Vec::new(), unreachable_masters: 0, cfg, opts: opts.clone() };
        let myself = records.iter().find(|r| r.flags.myself)?;
        let rec = Record { host: entry.host.clone(), port: entry.port, ..myself.clone() };
        cluster.nodes.push(Node { rec, replicas: 0, signature, link });
        for friend in records.iter().filter(|r| !r.flags.myself) {
            cluster.add_friend(friend);
        }
        cluster.count_replicas();
        Some(cluster)
    }

    fn add_friend(&mut self, rec: &Record) {
        if rec.flags.noaddr || rec.flags.handshake {
            return;
        }
        let addr = Addr { host: rec.host.clone(), port: rec.port };
        let Some(mut link) = link::open(&self.opts, &addr) else {
            self.unreachable_masters += usize::from(rec.flags.master);
            return;
        };
        let Some((records, signature)) = nodes_of(&mut link, &addr, &self.cfg) else { return };
        let mut rec = rec.clone();
        if let Some(own) = records.into_iter().find(|r| r.flags.myself) {
            (rec.migrating, rec.importing) = (own.migrating, own.importing);
        }
        self.nodes.push(Node { rec, replicas: 0, signature, link });
    }

    fn count_replicas(&mut self) {
        let masters: Vec<Vec<u8>> =
            self.nodes.iter().filter_map(|n| n.rec.master.clone()).collect();
        for node in &mut self.nodes {
            node.replicas = masters.iter().filter(|m| **m == node.rec.id).count();
        }
    }
}

/// A node's `CLUSTER NODES`, and the slot ownership it describes.
fn nodes_of(link: &mut Conn, addr: &Addr, cfg: &Config) -> Option<(Vec<Record>, Vec<u8>)> {
    let reply = match link.request(&[b"CLUSTER", b"NODES"]) {
        Ok(reply) => reply,
        Err(e) => {
            let at = addr.shown();
            eprint_bytes(&[b"Node ", &at, b" replied with error:\n", e.text().as_bytes(), b"\n"]);
            return None;
        }
    };
    let Some(text) = link::text(&reply) else {
        // A node without cluster support refuses CLUSTER NODES.
        let at = addr.shown();
        let text = [&b"[ERR] Node "[..], &at, b" is not configured as a cluster node."].concat();
        super::log::line(cfg.color, super::log::Level::Err, &text);
        return None;
    };
    let records = nodes_text::parse(text);
    let signature = signature(&records);
    Some((records, signature))
}

/// `id:ranges|id:ranges`, sorted by id, over nodes that own slots.
fn signature(records: &[Record]) -> Vec<u8> {
    let mut owners: Vec<Vec<u8>> = records
        .iter()
        .filter(|r| !r.slots.is_empty())
        .map(|r| [r.id.as_slice(), b":", &super::slots::bracketed(&r.slots.ranges())].concat())
        .collect();
    owners.sort();
    owners.join(&b'|')
}

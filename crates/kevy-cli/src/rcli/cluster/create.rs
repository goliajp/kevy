//! `--cluster create host:port[@bus] ...`: check the nodes are empty cluster
//! nodes, plan masters and replicas, and on confirmation make it so.

use super::addr::Addr;
use super::config::Config;
use super::log::{self, Level};
use super::nodes_text;
use super::plan::{self, Note, Planned};
use super::slots::{SlotSet, bracketed};
use crate::rcli::conn::Conn;
use crate::rcli::opts::Opts;
use crate::rcli::session::eprint_bytes;
use kevy_resp::Reply;

/// A node the cluster is created from.
pub(crate) struct Fresh {
    pub(crate) addr: Addr,
    pub(crate) bus: Option<i32>,
    pub(crate) link: Conn,
    pub(crate) id: Vec<u8>,
    /// Slots it already claims.
    pub(crate) slots: SlotSet,
}

/// Run `create`; the exit code.
pub(crate) fn run(opts: &Opts, cfg: &Config, args: &[Vec<u8>]) -> u8 {
    let mut nodes = Vec::new();
    for arg in args {
        match open(opts, cfg, arg) {
            Some(node) => nodes.push(node),
            None => return 1,
        }
    }
    let replicas = cfg.replicas.max(0) as usize;
    if nodes.len() / (replicas + 1) < 3 {
        refuse_configuration(cfg, nodes.len(), replicas);
        return 1;
    }
    let hosts: Vec<&[u8]> = nodes.iter().map(|n| n.addr.host.as_slice()).collect();
    let planned = announce(cfg, &hosts, replicas, &nodes);
    show_plan(&nodes, &planned);
    if !cfg.yes && !super::ask::confirm(b"Can I set the above configuration?") {
        return 0;
    }
    super::join::apply(opts, cfg, &mut nodes, &planned)
}

/// Parse, connect and vet one node; `None` after saying why not.
fn open(opts: &Opts, cfg: &Config, arg: &[u8]) -> Option<Fresh> {
    let Some(colon) = arg.iter().rposition(|&b| b == b':') else {
        eprint_bytes(&[b"Invalid address format: ", arg, b"\n"]);
        return None;
    };
    let (host, rest) = (&arg[..colon], &arg[colon + 1..]);
    let (port, bus) = match rest.iter().position(|&b| b == b'@') {
        Some(at) => {
            (crate::rcli::cnum::atoi(&rest[..at]), Some(crate::rcli::cnum::atoi(&rest[at + 1..])))
        }
        None => (crate::rcli::cnum::atoi(rest), None),
    };
    let addr = Addr { host: host.to_vec(), port };
    let mut link = super::link::open(opts, &addr)?;
    let myself = vet(cfg, &mut link, &addr)?;
    Some(Fresh { addr, bus, link, id: myself.id, slots: myself.slots })
}

/// A cluster node that knows no other node and holds no key: its own
/// record, or `None` after saying which requirement failed.
pub(crate) fn vet(cfg: &Config, link: &mut Conn, addr: &Addr) -> Option<nodes_text::Record> {
    let at = addr.shown();
    let Some(myself) = own_record(link) else {
        let text = [&b"[ERR] Node "[..], &at, b" is not configured as a cluster node."].concat();
        log::line(cfg.color, Level::Err, &text);
        return None;
    };
    if !is_empty(link) {
        let text = [&b"[ERR] Node "[..], &at, b" is not empty. Either the node already knows other nodes (check with CLUSTER NODES) or contains some key in database 0."].concat();
        log::line(cfg.color, Level::Err, &text);
        return None;
    }
    Some(myself)
}

fn own_record(link: &mut Conn) -> Option<nodes_text::Record> {
    let reply = link.request(&[b"CLUSTER", b"NODES"]).ok()?;
    nodes_text::parse(super::link::text(&reply)?).into_iter().find(|r| r.flags.myself)
}

/// Knows no other node and holds no key in database 0.
fn is_empty(link: &mut Conn) -> bool {
    let known = match link.request(&[b"CLUSTER", b"INFO"]) {
        Ok(reply) => field(&reply, b"cluster_known_nodes:").is_some_and(|v| v == b"1"),
        Err(_) => false,
    };
    let keys = match link.request(&[b"INFO", b"keyspace"]) {
        Ok(reply) => field(&reply, b"db0:").is_some(),
        Err(_) => true,
    };
    known && !keys
}

/// The value after `name` on its line of an INFO-style reply.
pub(crate) fn field<'a>(reply: &'a Reply, name: &[u8]) -> Option<&'a [u8]> {
    let text = super::link::text(reply)?;
    text.split(|&b| b == b'\n')
        .find_map(|line| line.strip_prefix(name))
        .map(|v| v.strip_suffix(b"\r").unwrap_or(v))
}

fn refuse_configuration(cfg: &Config, nodes: usize, replicas: usize) {
    let lines = [
        "*** ERROR: Invalid configuration for cluster creation.".to_string(),
        "*** Redis Cluster requires at least 3 master nodes.".to_string(),
        format!("*** This is not possible with {nodes} nodes and {replicas} replicas per node."),
        format!("*** At least {} nodes are required.", 3 * (replicas + 1)),
    ];
    for l in lines {
        log::line(cfg.color, Level::Err, l.as_bytes());
    }
}

/// Plan, printing the allocation and the anti-affinity outcome.
fn announce(cfg: &Config, hosts: &[&[u8]], replicas: usize, nodes: &[Fresh]) -> Vec<Planned> {
    let text = format!(">>> Performing hash slots allocation on {} nodes...", nodes.len());
    log::line(cfg.color, Level::Info, text.as_bytes());
    let (mut planned, notes) = plan::plan(hosts, replicas);
    for note in notes {
        match note {
            Note::Slots(i, a, b) => {
                log::plain(format!("Master[{i}] -> Slots {a} - {b}").as_bytes())
            }
            Note::Replica(r, m) => log::plain(
                &[&b"Adding replica "[..], &nodes[r].addr.shown(), b" to ", &nodes[m].addr.shown()]
                    .concat(),
            ),
            Note::ExtraReplicas => log::plain(b"Adding extra replicas..."),
        }
    }
    if plan::affinity_score(hosts, &planned) != (0, 0) {
        log::line(
            cfg.color,
            Level::Info,
            b">>> Trying to optimize slaves allocation for anti-affinity",
        );
        plan::optimize(hosts, &mut planned);
        let text: &[u8] = match plan::affinity_score(hosts, &planned) {
            (0, 0) => b"[OK] Perfect anti-affinity obtained!",
            (0, _) => b"[WARNING] Some slaves of the same master are in the same host",
            _ => b"[WARNING] Some slaves are in the same host as their master",
        };
        let level = if text.starts_with(b"[OK]") { Level::Ok } else { Level::Warn };
        log::line(cfg.color, level, text);
    }
    planned
}

/// Each node in the order given: masters with their slots, replicas with
/// their master.
fn show_plan(nodes: &[Fresh], planned: &[Planned]) {
    for p in planned {
        let n = &nodes[p.node];
        let role: &[u8] = if p.replicates.is_some() { b"S" } else { b"M" };
        log::plain(&[role, b": ", &n.id, b" ", &n.addr.shown()].concat());
        if let Some(m) = p.replicates {
            log::plain(&[&b"   replicates "[..], &nodes[m].id].concat());
            continue;
        }
        let mut slots = n.slots.clone();
        if let Some((a, b)) = p.slots {
            (a..=b).for_each(|s| slots.insert(s));
        }
        let count = format!(" ({} slots) master", slots.count());
        log::plain(&[&b"   slots:"[..], &bracketed(&slots.ranges()), count.as_bytes()].concat());
    }
}

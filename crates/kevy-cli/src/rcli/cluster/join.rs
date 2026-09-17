//! `create` after confirmation: assign slots and epochs, introduce the nodes,
//! wait for them to agree, attach the replicas, and check the result.

use super::config::Config;
use super::create::Fresh;
use super::log::{self, Level};
use super::nodes_text;
use super::plan::Planned;
use super::topology::Cluster;
use crate::rcli::opts::Opts;
use crate::rcli::send::write_out;
use kevy_resp::Reply;
use std::time::{Duration, Instant};

/// Make the plan so; the exit code.
pub(crate) fn apply(opts: &Opts, cfg: &Config, nodes: &mut [Fresh], planned: &[Planned]) -> u8 {
    for p in planned {
        let Some((a, b)) = p.slots else { continue };
        let numbers: Vec<Vec<u8>> = (a..=b).map(|s| s.to_string().into_bytes()).collect();
        let mut argv: Vec<&[u8]> = vec![b"CLUSTER", b"ADDSLOTS"];
        argv.extend(numbers.iter().map(Vec::as_slice));
        if !answered(cfg, &mut nodes[p.node], &argv) {
            return 1;
        }
    }
    log::line(cfg.color, Level::Info, b">>> Nodes configuration updated");
    log::line(cfg.color, Level::Info, b">>> Assign a different config epoch to each node");
    for (i, n) in nodes.iter_mut().enumerate() {
        let epoch = (i + 1).to_string();
        // A node that already has an epoch keeps it; the cluster resolves a clash.
        let _ = n.link.request(&[b"CLUSTER", b"SET-CONFIG-EPOCH", epoch.as_bytes()]);
    }
    log::line(cfg.color, Level::Info, b">>> Sending CLUSTER MEET messages to join the cluster");
    if !meet(cfg, nodes) {
        return 1;
    }
    write_out(b"Waiting for the cluster to join\n");
    let count = nodes.len();
    wait(nodes, |views| {
        views.iter().all(|v| v.len() == count && v.iter().all(|r| !r.flags.handshake))
    });
    for p in planned {
        let Some(m) = p.replicates else { continue };
        let master = nodes[m].id.clone();
        if !answered(cfg, &mut nodes[p.node], &[b"CLUSTER", b"REPLICATE", &master]) {
            return 1;
        }
    }
    let ids: Vec<(Vec<u8>, Option<Vec<u8>>)> = planned
        .iter()
        .map(|p| (nodes[p.node].id.clone(), p.replicates.map(|m| nodes[m].id.clone())))
        .collect();
    wait(nodes, |views| views.iter().all(|v| roles_match(v, &ids)) && agree(views));
    write_out(b"\n");
    let Some(mut c) = Cluster::load(opts, cfg.clone(), &nodes[0].addr) else { return 1 };
    u8::from(!super::check::run(&mut c))
}

/// Send and require a non-error reply, printing the error otherwise.
fn answered(cfg: &Config, n: &mut Fresh, argv: &[&[u8]]) -> bool {
    let why = match n.link.request(argv) {
        Ok(Reply::Error(e)) => e,
        Ok(_) => return true,
        Err(e) => e.text().into_bytes(),
    };
    let text = [&b"Node "[..], &n.addr.shown(), b" replied with error:\n", &why].concat();
    log::line(cfg.color, Level::Err, &text);
    false
}

/// Every node but the first meets the first, by address.
fn meet(cfg: &Config, nodes: &mut [Fresh]) -> bool {
    let Some((first, rest)) = nodes.split_first_mut() else { return true };
    let Some(ip) = resolve(&first.addr.host, first.addr.port) else {
        let text = [&b"Invalid IP address or hostname specified: "[..], &first.addr.host].concat();
        log::line(cfg.color, Level::Err, &text);
        return false;
    };
    let port = first.addr.port.to_string();
    let bus = first.bus.map(|b| b.to_string());
    for n in rest {
        let mut argv: Vec<&[u8]> = vec![b"CLUSTER", b"MEET", ip.as_bytes(), port.as_bytes()];
        if let Some(bus) = &bus {
            argv.push(bus.as_bytes());
        }
        if !answered(cfg, n, &argv) {
            return false;
        }
    }
    true
}

/// CLUSTER MEET takes an address, not a name.
fn resolve(host: &[u8], port: i32) -> Option<String> {
    use std::net::ToSocketAddrs;
    let host = std::str::from_utf8(host).ok()?;
    let port = u16::try_from(port).ok()?;
    (host, port).to_socket_addrs().ok()?.next().map(|a| a.ip().to_string())
}

/// Every node sees the same owner for every slot.
fn agree(views: &[Vec<nodes_text::Record>]) -> bool {
    views.windows(2).all(|w| super::topology::signature(&w[0]) == super::topology::signature(&w[1]))
}

fn roles_match(view: &[nodes_text::Record], ids: &[(Vec<u8>, Option<Vec<u8>>)]) -> bool {
    ids.iter().all(|(id, master)| view.iter().any(|r| r.id == *id && r.master == *master))
}

/// Poll every node's CLUSTER NODES until `done` holds, a dot a second.
fn wait(nodes: &mut [Fresh], done: impl Fn(&[Vec<nodes_text::Record>]) -> bool) {
    let mut last_dot = Instant::now();
    loop {
        let views: Vec<Vec<nodes_text::Record>> = nodes
            .iter_mut()
            .map(|n| match n.link.request(&[b"CLUSTER", b"NODES"]) {
                Ok(reply) => super::link::text(&reply).map(nodes_text::parse).unwrap_or_default(),
                Err(_) => Vec::new(),
            })
            .collect();
        if done(&views) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
        if last_dot.elapsed() >= Duration::from_secs(1) {
            write_out(b".");
            last_dot = Instant::now();
        }
    }
}

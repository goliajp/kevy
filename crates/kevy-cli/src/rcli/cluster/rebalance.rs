//! `--cluster rebalance`: move slots until each master owns its weight's
//! share of them.

use super::addr;
use super::config::Config;
use super::log::{self, Level};
use super::migrate::{self, Progress};
use super::slots::SLOTS;
use super::topology::Cluster;
use crate::rcli::opts::Opts;
use crate::rcli::send::write_out;

/// Run `rebalance`; the exit code.
pub(crate) fn run(opts: &Opts, cfg: Config, args: &[Vec<u8>]) -> u8 {
    let Some(entry) = addr::entry(args) else { return addr::report_invalid() };
    let Some(mut c) = Cluster::load(opts, cfg, &entry) else { return 1 };
    let Some(weights) = weights(&c) else { return 1 };
    if !super::check::run_with(&mut c, super::check::Listing::Hide) {
        let text = b"*** Please fix your cluster problems before rebalancing";
        log::line(c.cfg.color, Level::Err, text);
        return 1;
    }
    let involved: Vec<usize> = (0..c.nodes.len())
        .filter(|&i| {
            c.nodes[i].is_master() && (c.cfg.use_empty_masters || !c.nodes[i].rec.slots.is_empty())
        })
        .collect();
    let total: f64 = involved.iter().map(|&i| weights[i]).sum();
    if !beyond_threshold(&c, &involved, &weights, total) {
        let text = format!(
            "*** No rebalancing needed! All nodes are within the {:.2}% threshold.",
            c.cfg.threshold
        );
        log::line(c.cfg.color, Level::Warn, text.as_bytes());
        return 0;
    }
    let text =
        format!(">>> Rebalancing across {} nodes. Total weight = {total:.2}", involved.len());
    log::line(c.cfg.color, Level::Info, text.as_bytes());
    let mut order = balances(&c, &involved, &weights, total);
    order.sort_by_key(|&(_, balance)| balance);
    if c.cfg.verbose {
        for &(i, balance) in &order {
            log::plain(
                &[&c.nodes[i].shown()[..], format!(" balance is {balance} slots").as_bytes()]
                    .concat(),
            );
        }
    }
    u8::from(!settle(&mut c, &mut order))
}

/// Each loaded node's weight (1 unless `--cluster-weight` gives one by id
/// prefix); `None` after naming a prefix that matches no master.
fn weights(c: &Cluster) -> Option<Vec<f64>> {
    let mut weights = vec![1.0; c.nodes.len()];
    for pair in &c.cfg.weights {
        let eq = pair.iter().position(|&b| b == b'=').unwrap_or(pair.len());
        let (prefix, value) = (&pair[..eq], pair.get(eq + 1..).unwrap_or_default());
        let Some(i) = c.nodes.iter().position(|n| n.is_master() && n.rec.id.starts_with(prefix))
        else {
            log::line(
                c.cfg.color,
                Level::Err,
                &[&b"*** No such master node "[..], prefix].concat(),
            );
            return None;
        };
        weights[i] = crate::rcli::cnum::atof(value);
    }
    Some(weights)
}

/// Some node is off its share by more than the threshold, as a percentage
/// of what it owns now (a node that owns nothing but should is always off).
fn beyond_threshold(c: &Cluster, involved: &[usize], weights: &[f64], total: f64) -> bool {
    involved.iter().any(|&i| {
        let expected = SLOTS as f64 * weights[i] / total;
        let owned = c.nodes[i].rec.slots.count() as f64;
        if owned == 0.0 {
            return expected > 0.0;
        }
        (expected - owned).abs() / owned * 100.0 > c.cfg.threshold
    })
}

/// Slots over (positive) or under (negative) each node's share. Shares are
/// rounded down; the slots that leaves over are charged to the short nodes
/// in table order, one each.
fn balances(c: &Cluster, involved: &[usize], weights: &[f64], total: f64) -> Vec<(usize, i64)> {
    let mut out: Vec<(usize, i64)> = involved
        .iter()
        .map(|&i| {
            let expected = (SLOTS as f64 * weights[i] / total).floor() as i64;
            (i, c.nodes[i].rec.slots.count() as i64 - expected)
        })
        .collect();
    let mut surplus: i64 = out.iter().map(|(_, b)| b).sum();
    while surplus > 0 {
        for (_, balance) in out.iter_mut().filter(|(_, b)| *b < 0) {
            if surplus == 0 {
                break;
            }
            *balance -= 1;
            surplus -= 1;
        }
    }
    out
}

/// Pair the most short node with the most over one until all are even.
fn settle(c: &mut Cluster, order: &mut [(usize, i64)]) -> bool {
    let (mut dst, mut src) = (0, order.len().saturating_sub(1));
    while dst < src {
        let n = (-order[dst].1).min(order[src].1);
        if n > 0 && !move_between(c, order[src].0, order[dst].0, n as usize) {
            return false;
        }
        order[dst].1 += n;
        order[src].1 -= n;
        if order[dst].1 >= 0 {
            dst += 1;
        }
        if order[src].1 <= 0 {
            src = src.saturating_sub(1);
        }
    }
    true
}

fn move_between(c: &mut Cluster, source: usize, target: usize, n: usize) -> bool {
    let text = [
        format!("Moving {n} slots from ").as_bytes(),
        &c.nodes[source].shown(),
        b" to ",
        &c.nodes[target].shown(),
        b"\n",
    ]
    .concat();
    write_out(&text);
    let owned = [(source, c.nodes[source].rec.slots.iter().collect::<Vec<u16>>())];
    let moves = super::move_plan::plan(&owned, n);
    let ok = if c.cfg.simulate {
        write_out(&vec![b'#'; moves.len()]);
        true
    } else {
        migrate::move_slots(c, target, &moves, Progress::Hashes)
    };
    for &(_, slot) in &moves {
        c.nodes[source].rec.slots.remove(slot);
        c.nodes[target].rec.slots.insert(slot);
    }
    write_out(b"\n");
    ok
}

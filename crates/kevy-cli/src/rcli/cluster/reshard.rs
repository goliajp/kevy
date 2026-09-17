//! `--cluster reshard`: ask (or take from flags) how many slots, to which
//! master, from which masters; show the plan; move the slots.

use super::addr;
use super::config::Config;
use super::log::{self, Level};
use super::topology::Cluster;
use crate::rcli::opts::Opts;
use crate::rcli::send::write_out;

/// Run `reshard`; the exit code.
pub(crate) fn run(opts: &Opts, cfg: Config, args: &[Vec<u8>]) -> u8 {
    let Some(entry) = addr::entry(args) else { return addr::report_invalid() };
    let Some(mut c) = Cluster::load(opts, cfg, &entry) else { return 1 };
    if !super::check::run(&mut c) {
        crate::rcli::session::eprint_bytes(&[
            b"*** Please fix your cluster problems before resharding\n",
        ]);
        return 1;
    }
    let Some(wanted) = slot_count(&c) else { return 1 };
    let Some(target) = receiver(&c) else { return 1 };
    let Some(sources) = super::reshard_sources::sources(&c, target) else { return 1 };
    let owned: Vec<(usize, Vec<u16>)> =
        sources.iter().map(|&i| (i, c.nodes[i].rec.slots.iter().collect())).collect();
    let moves = super::move_plan::plan(&owned, wanted);
    show_plan(&c, &sources, target, &moves, wanted);
    if !c.cfg.yes && !proceed() {
        return 1;
    }
    u8::from(!super::migrate::move_slots(&mut c, target, &moves))
}

/// `--cluster-slots`, or asked until it is between 1 and 16384.
fn slot_count(c: &Cluster) -> Option<usize> {
    let max = super::slots::SLOTS as i32;
    let mut n = c.cfg.slots;
    while n <= 0 || n > max {
        let line = super::ask::line(b"How many slots do you want to move (from 1 to 16384)? ")?;
        n = crate::rcli::cnum::atoi(&line);
    }
    Some(n as usize)
}

/// `--cluster-to`, or asked once; it must name a master.
fn receiver(c: &Cluster) -> Option<usize> {
    let id = match &c.cfg.to {
        Some(id) => id.clone(),
        None => super::ask::line(b"What is the receiving node ID? ")?,
    };
    let found = master_by_id(c, &id);
    if found.is_none() {
        not_a_master(c, &id);
    }
    found
}

/// A loaded master with this id.
pub(crate) fn master_by_id(c: &Cluster, id: &[u8]) -> Option<usize> {
    c.nodes.iter().position(|n| n.is_master() && n.rec.id.eq_ignore_ascii_case(id))
}

pub(crate) fn not_a_master(c: &Cluster, id: &[u8]) {
    let text =
        [&b"*** The specified node ("[..], id, b") is not known or not a master, please retry."]
            .concat();
    log::line(c.cfg.color, Level::Err, &text);
}

fn show_plan(c: &Cluster, sources: &[usize], target: usize, moves: &[(usize, u16)], wanted: usize) {
    write_out(format!("\nReady to move {wanted} slots.\n  Source nodes:\n").as_bytes());
    for &s in sources {
        super::show::node_indented(&c.nodes[s], b"    ");
    }
    write_out(b"  Destination node:\n");
    super::show::node_indented(&c.nodes[target], b"    ");
    write_out(b"  Resharding plan:\n");
    for &(s, slot) in moves {
        let text = [format!("    Moving slot {slot} from ").as_bytes(), &c.nodes[s].rec.id, b"\n"]
            .concat();
        write_out(&text);
    }
}

/// `yes` goes ahead; anything else, or no answer, does not.
fn proceed() -> bool {
    super::ask::line(b"Do you want to proceed with the proposed reshard plan (yes/no)? ")
        .is_some_and(|l| l == b"yes")
}

//! `--cluster check`: whether the nodes agree, which slots are open, and
//! whether every slot has an owner.

use super::log::{self, Level};
use super::slots::{SLOTS, SlotSet};
use super::topology::Cluster;

/// Whether the check lists the nodes first.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Listing {
    Show,
    Hide,
}

/// Print the check; `true` when it found nothing wrong.
pub(crate) fn run(c: &mut Cluster) -> bool {
    run_with(c, Listing::Show)
}

/// [`run`], with or without the node listing.
pub(crate) fn run_with(c: &mut Cluster, listing: Listing) -> bool {
    let color = c.cfg.color;
    let entry = c.nodes.first().map(|n| n.shown()).unwrap_or_default();
    log::line(
        color,
        Level::Info,
        &[b">>> Performing Cluster Check (using node ", &entry[..], b")"].concat(),
    );
    if listing == Listing::Show {
        for n in &c.nodes {
            super::show::node(n);
        }
    }
    let mut ok = agreement(c);
    ok &= open_slots(c);
    ok &= coverage(c);
    if c.cfg.search_multiple_owners {
        ok &= super::owners::report(c);
    }
    ok
}

fn agreement(c: &Cluster) -> bool {
    let first = c.nodes.first().map(|n| &n.signature);
    let agree = c.nodes.iter().all(|n| Some(&n.signature) == first);
    if agree {
        log::line(c.cfg.color, Level::Ok, b"[OK] All nodes agree about slots configuration.");
    } else {
        log::line(c.cfg.color, Level::Err, b"[ERR] Nodes don't agree about configuration!");
    }
    agree
}

fn open_slots(c: &Cluster) -> bool {
    let color = c.cfg.color;
    log::line(color, Level::Info, b">>> Check for open slots...");
    let mut open: Vec<u16> = Vec::new();
    for n in &c.nodes {
        for (state, list) in
            [(&b"migrating"[..], &n.rec.migrating), (b"importing", &n.rec.importing)]
        {
            if list.is_empty() {
                continue;
            }
            let slots = joined(list.iter().map(|(s, _)| *s));
            let text = [
                b"[WARNING] Node ",
                &n.shown()[..],
                b" has slots in ",
                state,
                b" state ",
                &slots,
                b".",
            ];
            log::line(color, Level::Err, &text.concat());
            for &(slot, _) in list.iter() {
                if !open.contains(&slot) {
                    open.push(slot);
                }
            }
        }
    }
    if open.is_empty() {
        return true;
    }
    open.sort_unstable();
    let text = [&b"[WARNING] The following slots are open: "[..], &joined(open.into_iter()), b"."];
    log::line(color, Level::Err, &text.concat());
    false
}

fn coverage(c: &Cluster) -> bool {
    let color = c.cfg.color;
    log::line(color, Level::Info, b">>> Check slots coverage...");
    let covered = covered(c).count();
    if covered == SLOTS {
        log::line(color, Level::Ok, format!("[OK] All {SLOTS} slots covered.").as_bytes());
        return true;
    }
    log::line(
        color,
        Level::Err,
        format!("[ERR] Not all {SLOTS} slots are covered by nodes.\n").as_bytes(),
    );
    false
}

/// The slots some loaded node owns.
pub(crate) fn covered(c: &Cluster) -> SlotSet {
    let mut all = SlotSet::default();
    for n in &c.nodes {
        for s in n.rec.slots.iter() {
            all.insert(s);
        }
    }
    all
}

/// `5,6,7`.
pub(crate) fn joined(slots: impl Iterator<Item = u16>) -> Vec<u8> {
    slots.map(|s| s.to_string()).collect::<Vec<_>>().join(",").into_bytes()
}

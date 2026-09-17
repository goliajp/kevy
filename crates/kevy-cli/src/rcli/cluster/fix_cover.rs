//! Covering slots no master owns: by where their keys are, each group of
//! slots behind its own confirmation (which --cluster-yes does not answer).

use super::log::{self, Level};
use super::slots::{SLOTS, SlotSet, bracketed};
use super::topology::Cluster;

/// Uncovered slots, grouped by how many masters hold keys in them.
struct Groups {
    none: Vec<u16>,
    /// `(slot, the one holder)`.
    one: Vec<(u16, usize)>,
    /// `(slot, the holders)`.
    many: Vec<(u16, Vec<usize>)>,
}

/// Cover every uncovered slot; `false` when the user declines a group.
pub(crate) fn fix(c: &mut Cluster) -> bool {
    log::line(c.cfg.color, Level::Info, b">>> Fixing slots coverage...");
    let groups = group(c);
    if !groups.none.is_empty() {
        if !ask(&groups.none, b"have no keys across the cluster", b"covering with a random node") {
            return false;
        }
        for &slot in &groups.none {
            let Some(node) = fewest_slots(c) else { break };
            cover(c, node, slot, b"with");
        }
    }
    let one: Vec<u16> = groups.one.iter().map(|(s, _)| *s).collect();
    if !one.is_empty() {
        if !ask(&one, b"have keys in just one node", b"covering with those nodes") {
            return false;
        }
        for &(slot, node) in &groups.one {
            cover(c, node, slot, b"with");
        }
    }
    let many: Vec<u16> = groups.many.iter().map(|(s, _)| *s).collect();
    if !many.is_empty() {
        if !ask(&many, b"have keys in multiple nodes", b"moving keys into a single node") {
            return false;
        }
        for (slot, holders) in &groups.many {
            gather(c, *slot, holders);
        }
    }
    true
}

fn group(c: &mut Cluster) -> Groups {
    let covered = super::check::covered(c);
    let masters: Vec<usize> = (0..c.nodes.len()).filter(|&i| c.nodes[i].is_master()).collect();
    let mut groups = Groups { none: Vec::new(), one: Vec::new(), many: Vec::new() };
    for slot in (0..SLOTS as u16).filter(|&s| !covered.contains(s)) {
        let holders: Vec<usize> =
            masters.iter().copied().filter(|&m| super::owner::keys_in(c, m, slot) > 0).collect();
        match holders.as_slice() {
            [] => groups.none.push(slot),
            [only] => groups.one.push((slot, *only)),
            _ => groups.many.push((slot, holders)),
        }
    }
    groups
}

/// List the slots and ask; only `yes` goes ahead.
fn ask(slots: &[u16], what: &[u8], how: &[u8]) -> bool {
    let mut set = SlotSet::default();
    slots.iter().for_each(|&s| set.insert(s));
    log::plain(&[&b"The following uncovered slots "[..], what, b":"].concat());
    log::plain(&bracketed(&set.ranges()));
    super::ask::confirm(&[&b"Fix these slots by "[..], how, b"?"].concat())
}

/// The master owning fewest slots, the lowest id on a tie: the same choice on
/// every run, and the cover spreads instead of piling onto one node.
fn fewest_slots(c: &Cluster) -> Option<usize> {
    (0..c.nodes.len()).filter(|&i| c.nodes[i].is_master()).min_by(|&a, &b| {
        let key = |i: usize| (c.nodes[i].rec.slots.count(), c.nodes[i].rec.id.clone());
        key(a).cmp(&key(b))
    })
}

fn cover(c: &mut Cluster, node: usize, slot: u16, how: &[u8]) -> bool {
    let text = [format!(">>> Covering slot {slot} ").as_bytes(), how, b" ", &c.nodes[node].shown()]
        .concat();
    log::line(c.cfg.color, Level::Info, &text);
    match super::owner::set(c, node, slot) {
        Ok(()) => true,
        Err(why) => {
            super::migrate::node_error(c, node, &why);
            false
        }
    }
}

/// Give the slot to the holder with most keys, then move the others' keys
/// in. A holder serves MIGRATE only for a slot it believes it owns, so each
/// takes the slot for the move and hands it to the target after.
fn gather(c: &mut Cluster, slot: u16, holders: &[usize]) {
    let Some(target) = super::owner::most_keys(c, holders, slot) else { return };
    if !cover(c, target, slot, b"moving keys to") {
        return;
    }
    let number = slot.to_string();
    let target_id = c.nodes[target].rec.id.clone();
    for &from in holders.iter().filter(|&&h| h != target) {
        let own_id = c.nodes[from].rec.id.clone();
        let _ = c.nodes[from].link.request(&[
            b"CLUSTER",
            b"SETSLOT",
            number.as_bytes(),
            b"NODE",
            &own_id,
        ]); // a refusal shows up as MIGRATE's error below
        let moved = super::migrate_slot::move_keys_only(c, (from, target), slot);
        let _ = c.nodes[from].link.request(&[
            b"CLUSTER",
            b"SETSLOT",
            number.as_bytes(),
            b"NODE",
            &target_id,
        ]); // the target's bumped epoch settles it either way
        if !moved {
            return;
        }
    }
}

//! Closing an open slot: settle who owns it, then finish or undo the
//! migration its marks describe.

use super::log::{self, Level};
use super::topology::Cluster;

/// The nodes that mark `slot` as migrating and as importing, in table order.
fn marked(c: &Cluster, slot: u16) -> (Vec<usize>, Vec<usize>) {
    let has = |list: &[(u16, Vec<u8>)]| list.iter().any(|(s, _)| *s == slot);
    let migrating = (0..c.nodes.len()).filter(|&i| has(&c.nodes[i].rec.migrating)).collect();
    let importing = (0..c.nodes.len()).filter(|&i| has(&c.nodes[i].rec.importing)).collect();
    (migrating, importing)
}

fn addresses(c: &Cluster, nodes: &[usize]) -> Vec<u8> {
    nodes.iter().map(|&i| c.nodes[i].shown()).collect::<Vec<_>>().join(&b',')
}

/// Fix one open slot; `false` when this client cannot.
pub(crate) fn fix(c: &mut Cluster, slot: u16) -> bool {
    let color = c.cfg.color;
    log::line(color, Level::Info, format!(">>> Fixing open slot {slot}").as_bytes());
    let masters: Vec<usize> = (0..c.nodes.len()).filter(|&i| c.nodes[i].is_master()).collect();
    let claimers: Vec<usize> =
        masters.iter().copied().filter(|&i| c.nodes[i].rec.slots.contains(slot)).collect();
    let holders: Vec<usize> =
        masters.iter().copied().filter(|&i| super::owner::keys_in(c, i, slot) > 0).collect();
    for &i in holders.iter().filter(|i| !claimers.contains(i)) {
        let text = [
            format!("*** Found keys about slot {slot} in non-owner node ").as_bytes(),
            &c.nodes[i].shown(),
            b"!",
        ]
        .concat();
        log::line(color, Level::Warn, &text);
    }
    let (migrating, mut importing) = marked(c, slot);
    if !migrating.is_empty() {
        log::plain(&[&b"Set as migrating in: "[..], &addresses(c, &migrating)].concat());
    }
    if !importing.is_empty() {
        log::plain(&[&b"Set as importing in: "[..], &addresses(c, &importing)].concat());
    }
    let mut owners: Vec<usize> = claimers.clone();
    owners.extend(holders.iter().copied().filter(|i| !claimers.contains(i)));
    let Some(owner) = settle_owner(c, slot, &owners, &mut importing) else { return false };
    importing.retain(|&i| i != owner);
    super::fix_open_cases::close(c, slot, owner, &migrating, &importing)
}

/// The one owner, or the node with most keys made owner, the others then
/// importing from it.
fn settle_owner(
    c: &mut Cluster,
    slot: u16,
    owners: &[usize],
    importing: &mut Vec<usize>,
) -> Option<usize> {
    if let [only] = owners {
        return Some(*only);
    }
    let color = c.cfg.color;
    log::line(
        color,
        Level::Info,
        b">>> No single clear owner for the slot, selecting an owner by # of keys...",
    );
    let candidates: Vec<usize> = if owners.is_empty() {
        (0..c.nodes.len()).filter(|&i| c.nodes[i].is_master()).collect()
    } else {
        owners.to_vec()
    };
    let owner = super::owner::most_keys(c, &candidates, slot)?;
    let text = [&b"*** Configuring "[..], &c.nodes[owner].shown(), b" as the slot owner"].concat();
    log::line(color, Level::Warn, &text);
    if let Err(why) = super::owner::set(c, owner, slot) {
        super::migrate::node_error(c, owner, &why);
        return None;
    }
    let owner_id = c.nodes[owner].rec.id.clone();
    let number = slot.to_string();
    for &i in owners.iter().filter(|&&i| i != owner) {
        let _ = c.nodes[i].link.request(&[
            b"CLUSTER",
            b"SETSLOT",
            number.as_bytes(),
            b"IMPORTING",
            &owner_id,
        ]); // a refusal leaves it as it was; its keys still move below
        if !importing.contains(&i) {
            importing.push(i);
        }
    }
    Some(owner)
}

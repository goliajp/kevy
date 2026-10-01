//! The four ways an open slot is closed, once its owner is settled.

use super::log::{self, Level};
use super::migrate::Progress;
use super::topology::Cluster;

/// Close `slot` according to its marks; `false` when no case applies.
pub(crate) fn close(
    c: &mut Cluster,
    slot: u16,
    owner: usize,
    migrating: &[usize],
    importing: &[usize],
) -> bool {
    match (migrating, importing) {
        ([m], [i]) if *m == owner => move_to(c, slot, owner, *i),
        ([], [_, ..]) => keys_home(c, slot, owner, importing),
        ([m], [_, _, ..]) if *m == owner => move_and_close(c, slot, owner, importing),
        ([m], []) if *m == owner => {
            let text = [
                format!(">>> Case 4: Closing slot {slot} on ").as_bytes(),
                &c.nodes[owner].shown(),
            ]
            .concat();
            log::line(c.cfg.color, Level::Info, &text);
            stable(c, owner, slot, false)
        }
        _ => {
            cannot(c, owner, migrating, importing);
            false
        }
    }
}

/// Case 1: finish the migration to the one importing node.
fn move_to(c: &mut Cluster, slot: u16, owner: usize, target: usize) -> bool {
    let text = [
        format!(">>> Case 1: Moving slot {slot} from ").as_bytes(),
        &c.nodes[owner].shown(),
        b" to ",
        &c.nodes[target].shown(),
    ]
    .concat();
    log::line(c.cfg.color, Level::Info, &text);
    super::migrate_slot::move_slot(c, (owner, target), slot, Progress::Steps)
}

/// Case 2: nothing is migrating; bring the importing nodes' keys back.
fn keys_home(c: &mut Cluster, slot: u16, owner: usize, importing: &[usize]) -> bool {
    let text = [
        format!(">>> Case 2: Moving all the {slot} slot keys to its owner ").as_bytes(),
        &c.nodes[owner].shown(),
    ]
    .concat();
    log::line(c.cfg.color, Level::Info, &text);
    importing.iter().all(|&i| {
        super::migrate_slot::move_keys_only(c, (i, owner), slot) && stable(c, i, slot, true)
    })
}

/// Case 3: finish the migration the owner's mark names, close the others.
fn move_and_close(c: &mut Cluster, slot: u16, owner: usize, importing: &[usize]) -> bool {
    let target = pointed_target(c, owner, slot, importing);
    let text = [
        format!(">>> Case 3: Moving slot {slot} from ").as_bytes(),
        &c.nodes[owner].shown(),
        b" to ",
        &c.nodes[target].shown(),
        b" and closing it on all the other importing nodes.",
    ]
    .concat();
    log::line(c.cfg.color, Level::Info, &text);
    if !super::migrate_slot::move_slot(c, (owner, target), slot, Progress::Steps) {
        return false;
    }
    importing.iter().filter(|&&i| i != target).all(|&i| stable(c, i, slot, false))
}

/// The importing node the owner's migrating mark points at, else the first.
fn pointed_target(c: &Cluster, owner: usize, slot: u16, importing: &[usize]) -> usize {
    let pointed =
        c.nodes[owner].rec.migrating.iter().find(|(s, _)| *s == slot).map(|(_, id)| id.clone());
    importing
        .iter()
        .copied()
        .find(|&i| Some(&c.nodes[i].rec.id) == pointed.as_ref())
        .unwrap_or(importing[0])
}

/// SETSLOT STABLE on `node`, announced when `say`.
fn stable(c: &mut Cluster, node: usize, slot: u16, say: bool) -> bool {
    if say {
        let text = [format!(">>> Setting {slot} as STABLE in ").as_bytes(), &c.nodes[node].shown()]
            .concat();
        log::line(c.cfg.color, Level::Info, &text);
    }
    let number = slot.to_string();
    match super::migrate::failure(&c.nodes[node].link.request(&[
        b"CLUSTER",
        b"SETSLOT",
        number.as_bytes(),
        b"STABLE",
    ])) {
        None => true,
        Some(why) => {
            super::migrate::node_error(c, node, &why);
            false
        }
    }
}

fn cannot(c: &Cluster, owner: usize, migrating: &[usize], importing: &[usize]) {
    let list =
        |nodes: &[usize]| nodes.iter().map(|&i| c.nodes[i].shown()).collect::<Vec<_>>().join(&b',');
    let text = [
        &b"[ERR] Sorry, kevy-cli can't fix this slot yet (work in progress). Slot is set as migrating in "[..],
        &list(migrating),
        b", as importing in ",
        &list(importing),
        b", owner is ",
        &c.nodes[owner].shown(),
    ]
    .concat();
    log::line(c.cfg.color, Level::Err, &text);
}

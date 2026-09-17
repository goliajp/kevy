//! `--cluster-search-multiple-owners`: slots that more than one master owns
//! or holds keys in.

use super::log::{self, Level};
use super::slots::SLOTS;
use super::topology::Cluster;
use kevy_resp::Reply;

/// For each slot, the masters that own it or hold keys in it.
pub(crate) fn find(c: &mut Cluster) -> Vec<(u16, Vec<usize>)> {
    let mut owners: Vec<Vec<usize>> = vec![Vec::new(); SLOTS];
    for (i, n) in c.nodes.iter_mut().enumerate().filter(|(_, n)| n.is_master()) {
        let numbers: Vec<Vec<u8>> = (0..SLOTS).map(|s| s.to_string().into_bytes()).collect();
        let commands: Vec<Vec<&[u8]>> =
            numbers.iter().map(|s| vec![&b"CLUSTER"[..], b"COUNTKEYSINSLOT", s]).collect();
        let counts = n.link.pipeline(&commands).unwrap_or_default();
        for (slot, list) in owners.iter_mut().enumerate() {
            let has_keys = matches!(counts.get(slot), Some(Reply::Int(k)) if *k > 0);
            if has_keys || n.rec.slots.contains(slot as u16) {
                list.push(i);
            }
        }
    }
    owners
        .into_iter()
        .enumerate()
        .filter(|(_, list)| list.len() > 1)
        .map(|(slot, list)| (slot as u16, list))
        .collect()
}

/// Print what [`find`] finds; `true` when every slot has at most one owner.
pub(crate) fn report(c: &mut Cluster) -> bool {
    let color = c.cfg.color;
    log::line(color, Level::Info, b">>> Check for multiple slot owners...");
    let found = find(c);
    for (slot, list) in &found {
        let text = format!("[WARNING] Slot {slot} has {} owners:", list.len());
        log::line(color, Level::Err, text.as_bytes());
        for &i in list {
            log::plain(&[b"    ", &c.nodes[i].shown()[..]].concat());
        }
    }
    if found.is_empty() {
        log::line(color, Level::Ok, b"[OK] No multiple owners found.");
    }
    found.is_empty()
}

//! Giving a slot to one master, and counting a slot's keys on a node.

use super::topology::Cluster;
use kevy_resp::Reply;

/// Make `node` the slot's owner: drop and re-add it and bump the node's
/// epoch in one transaction, so the claim wins over older ones.
pub(crate) fn set(c: &mut Cluster, node: usize, slot: u16) -> Result<(), Vec<u8>> {
    let number = slot.to_string();
    let n = number.as_bytes();
    let commands: Vec<Vec<&[u8]>> = vec![
        vec![b"MULTI"],
        vec![b"CLUSTER", b"DELSLOTS", n],
        vec![b"CLUSTER", b"ADDSLOTS", n],
        vec![b"CLUSTER", b"BUMPEPOCH"],
        vec![b"EXEC"],
    ];
    let replies = c.nodes[node].link.pipeline(&commands).map_err(|e| e.text().into_bytes())?;
    if let Some(Reply::Error(e)) = replies.last() {
        return Err(e.clone());
    }
    for (i, other) in c.nodes.iter_mut().enumerate() {
        if i == node {
            other.rec.slots.insert(slot);
        } else {
            other.rec.slots.remove(slot);
        }
    }
    Ok(())
}

/// CLUSTER COUNTKEYSINSLOT on `node`; 0 when it cannot say.
pub(crate) fn keys_in(c: &mut Cluster, node: usize, slot: u16) -> i64 {
    let number = slot.to_string();
    match c.nodes[node].link.request(&[b"CLUSTER", b"COUNTKEYSINSLOT", number.as_bytes()]) {
        Ok(Reply::Int(n)) => n,
        _ => 0,
    }
}

/// The index in `candidates` with most keys in `slot`, the first on a tie.
pub(crate) fn most_keys(c: &mut Cluster, candidates: &[usize], slot: u16) -> Option<usize> {
    let counts: Vec<i64> = candidates.iter().map(|&n| keys_in(c, n, slot)).collect();
    let best = counts.iter().copied().max()?;
    counts.iter().position(|&k| k == best).map(|i| candidates[i])
}

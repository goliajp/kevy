//! Moving slots between masters: atomically when every node supports it
//! (CLUSTER MIGRATION, Redis 8.4 and later), otherwise slot by slot with
//! MIGRATE.

use super::log::{self, Level};
use super::topology::Cluster;
use kevy_resp::Reply;

/// Whether a move reports its steps (reshard) or only a `#` per slot
/// (rebalance).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Progress {
    Steps,
    Hashes,
}

/// Move `(source, slot)` pairs to `target`; `false` when a move failed
/// (after saying why).
pub(crate) fn move_slots(
    c: &mut Cluster,
    target: usize,
    moves: &[(usize, u16)],
    progress: Progress,
) -> bool {
    if atomic_everywhere(c) {
        return super::migrate_atomic::run(c, target, moves, progress);
    }
    moves
        .iter()
        .all(|&(source, slot)| super::migrate_slot::move_slot(c, (source, target), slot, progress))
}

/// Every loaded node reports a version with CLUSTER MIGRATION (8.4.0+).
fn atomic_everywhere(c: &mut Cluster) -> bool {
    c.nodes.iter_mut().all(|n| {
        let Ok(reply) = n.link.request(&[b"INFO", b"server"]) else { return false };
        super::create::field(&reply, b"redis_version:").is_some_and(|v| version(v) >= (8, 4, 0))
    })
}

/// `major.minor.patch`, missing parts 0.
fn version(text: &[u8]) -> (u32, u32, u32) {
    let mut parts = text
        .split(|&b| b == b'.')
        .map(|p| std::str::from_utf8(p).ok().and_then(|p| p.parse().ok()).unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

/// An error reply's text, or the link failure's.
pub(crate) fn failure(reply: &Result<Reply, crate::rcli::conn::LinkError>) -> Option<Vec<u8>> {
    match reply {
        Ok(Reply::Error(e)) => Some(e.clone()),
        Ok(_) => None,
        Err(e) => Some(e.text().into_bytes()),
    }
}

pub(crate) fn report(c: &Cluster, text: &[u8]) {
    log::line(c.cfg.color, Level::Err, text);
}

/// `Node host:port replied with error:` and the error on its own line.
pub(crate) fn node_error(c: &Cluster, node: usize, why: &[u8]) {
    let at = c.nodes[node].shown();
    crate::rcli::send::write_out(
        &[&b"Node "[..], &at, b" replied with error:\n", why, b"\n"].concat(),
    );
}

/// A step of a slot move failed: the error, then a blank line.
pub(crate) fn move_failed(c: &Cluster, node: usize, why: &[u8]) {
    crate::rcli::send::write_out(b"\n");
    node_error(c, node, why);
    crate::rcli::send::write_out(b"\n");
}

#[cfg(test)]
mod tests {
    #[test]
    fn versions_compare_by_number() {
        assert!(super::version(b"8.10.1") >= (8, 4, 0));
        assert!(super::version(b"8.4.0") >= (8, 4, 0));
        assert!(super::version(b"7.4.10") < (8, 4, 0));
        assert!(super::version(b"255.255.255") >= (8, 4, 0));
        assert_eq!(super::version(b"x"), (0, 0, 0));
    }
}

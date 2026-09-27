//! Single-node cluster topology: the advertised per-shard addresses and the
//! `-MOVED` redirect a cluster conn gets for a wrong-shard key.
//!
//! Scope guard: this is a *protocol carrier* for key-aware routing inside one
//! process — multi-node distribution, failover, MIGRATE/ASK and gossip are
//! permanently out of scope.

/// Advertised cluster addressing, shared by every shard when cluster mode is
/// on: shard `i` is reachable at `ip:(port_base + i)`.
#[derive(Clone)]
pub(crate) struct ClusterTopo {
    /// Advertised IPv4 address: the announce address when configured,
    /// else the bind address with `127.0.0.1` for a `0.0.0.0` bind.
    pub(crate) ip: [u8; 4],
    /// First advertised cluster port; shard `i` is reached at
    /// `port_base + i`.
    pub(crate) port_base: u16,
    /// First advertised ENCRYPTED cluster port, when the encrypted front
    /// end serves cluster ports; relayed clients are sent there.
    pub(crate) secure_port_base: Option<u16>,
}

impl ClusterTopo {
    /// `-MOVED <slot> <ip>:<port>\r\n` pointing at `shard`'s cluster port —
    /// its encrypted twin for a client the front end relays.
    pub(crate) fn moved(&self, slot: u16, shard: usize) -> Vec<u8> {
        let [a, b, c, d] = self.ip;
        let base = match self.secure_port_base {
            Some(s) if relayed_client() => s,
            _ => self.port_base,
        };
        format!("-MOVED {slot} {a}.{b}.{c}.{d}:{}\r\n", base as usize + shard).into_bytes()
    }
}

thread_local! {
    static RELAYED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Mark whether the command about to run on this shard thread came from a
/// relayed (encrypted) client.
pub(crate) fn set_relayed_client(relayed: bool) {
    RELAYED.with(|r| r.set(relayed));
}

/// Whether the command being dispatched on this thread came from a client
/// the encrypted front end relays (see `Runtime::with_peer_token`). Cluster
/// replies use it to advertise encrypted ports; it is only kept up to date
/// in cluster mode, and reads `false` otherwise.
///
/// ```
/// assert!(!kevy_rt::relayed_client());
/// ```
pub fn relayed_client() -> bool {
    RELAYED.with(std::cell::Cell::get)
}

/// The contiguous slot range `[start, end]` (inclusive, CLUSTER SLOTS shape)
/// shard `i` of `n` owns: `[ceil(i·16384/n), ceil((i+1)·16384/n) - 1]`.
/// Exact inverse of `reduce::slot_to_shard`'s multiply-shift.
pub fn shard_slot_range(i: usize, n: usize) -> (u16, u16) {
    let start = (i * 16384).div_ceil(n);
    let end = ((i + 1) * 16384).div_ceil(n) - 1;
    (start as u16, end as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reduce::slot_to_shard;

    #[test]
    fn ranges_partition_and_invert() {
        for n in [1usize, 2, 3, 5, 7, 8, 16, 100] {
            let mut next = 0u32;
            for i in 0..n {
                let (start, end) = shard_slot_range(i, n);
                assert_eq!(u32::from(start), next, "n={n} shard {i} contiguous");
                assert!(start <= end);
                for slot in [start, end] {
                    assert_eq!(slot_to_shard(slot, n), i, "n={n} slot {slot}");
                }
                next = u32::from(end) + 1;
            }
            assert_eq!(next, 16384, "n={n} covers all slots");
        }
    }

    #[test]
    fn moved_reply_shape() {
        let topo = ClusterTopo { ip: [127, 0, 0, 1], port_base: 6005, secure_port_base: None };
        assert_eq!(topo.moved(12182, 5), b"-MOVED 12182 127.0.0.1:6010\r\n".to_vec());
    }
}

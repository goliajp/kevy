//! Connection-lifecycle helpers split out of [`crate::shard`] so that
//! file stays under the 500-LOC project ceiling. Still the same
//! `impl Shard` — same private state, called from `run()` and the
//! conn-close paths in [`crate::inbox`].

use std::io;

use crate::Commands;
use crate::conn::Conn;
use crate::shard::Shard;

/// Which listener an accept came from. Three listeners share one
/// connection-setup body, and a boolean cannot carry three.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Accepted {
    /// The shared compat listener every client reaches.
    Compat,
    /// The per-shard cluster listener (conns marked for `-MOVED`).
    Cluster,
    /// The unix-domain listener, which lives on shard 0 only.
    Unix,
}

impl<C: Commands> Shard<C> {
    /// The socket a given accept event came from, if this shard holds
    /// one — off-accept-set shards have no compat listener, cluster mode
    /// may be off, and only shard 0 ever holds the unix listener.
    fn listener_for(&self, from: Accepted) -> Option<&kevy_sys::Socket> {
        match from {
            Accepted::Compat => self.listener.as_ref(),
            Accepted::Cluster => self.cluster_listener.as_ref(),
            Accepted::Unix => self.unix_listener.as_ref(),
        }
    }

    /// Drain one listener's accept queue.
    ///
    /// The selector is an enum rather than the `cluster: bool` it used to
    /// be: there are three listeners now, and the unix one arrived after
    /// the boolean did — which is how it ended up bound but never
    /// accepted on this reactor for a whole release.
    pub(crate) fn accept_ready(&mut self, from: Accepted) -> io::Result<()> {
        let cluster = from == Accepted::Cluster;
        loop {
            let Some(listener) = self.listener_for(from) else { return Ok(()) };
            let accepted = listener.accept();
            match accepted {
                Ok(sock) => {
                    // Refuse client conns past max_clients_per_shard
                    // (cluster-bus links exempt; they're infra, not user-counted).
                    if !cluster
                        && self.max_clients_per_shard > 0
                        && self.conns.len() >= self.max_clients_per_shard
                    {
                        self.rejected_connections = self.rejected_connections.saturating_add(1);
                        drop(sock); // close immediately; client sees EOF/RST.
                        continue;
                    }
                    sock.set_nonblocking()?;
                    // TCP_NODELAY doesn't apply to AF_UNIX; skip for UDS.
                    if from != Accepted::Unix {
                        let _ = sock.set_nodelay();
                    }
                    let fd = sock.raw();
                    let id = self.next_conn_id;
                    self.next_conn_id += self.conn_id_step;
                    self.poller.add(fd, true, false)?;
                    self.fd_to_conn.insert(fd, id);
                    let mut conn = Conn::new(sock);
                    conn.cluster = cluster;
                    self.conns.insert(id, conn);
                    // Client connections only — cluster-bus links are internal.
                    if !cluster {
                        self.commands.on_connection();
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {} // retry accept
                Err(_) => break,
            }
        }
        Ok(())
    }

    /// Drop a (closing) connection's subscriptions from the shared registry, so
    /// PUBLISH counts and the fan-out bitset don't count a gone subscriber.
    pub(crate) fn unregister_subs(&self, subs: &std::collections::HashSet<Vec<u8>>) {
        if subs.is_empty() {
            return;
        }
        let mut reg = self.pubsub.write().expect("pubsub registry");
        for ch in subs {
            let drop = match reg.get_mut(ch) {
                Some(e) => {
                    e.0 = e.0.saturating_sub(1);
                    e.0 == 0
                }
                None => false,
            };
            if drop {
                reg.remove(ch);
            }
        }
    }
}

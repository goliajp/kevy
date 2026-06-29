//! Connection-lifecycle helpers split out of [`crate::shard`] so that
//! file stays under the 500-LOC project ceiling. Still the same
//! `impl Shard` — same private state, called from `run()` and the
//! conn-close paths in [`crate::inbox`].

use std::io;

use crate::Commands;
use crate::conn::Conn;
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    /// Drain one listener's accept queue. `cluster` selects the per-shard
    /// cluster listener (conns marked for `-MOVED` semantics) over the
    /// shared compat listener.
    pub(crate) fn accept_ready(&mut self, cluster: bool) -> io::Result<()> {
        loop {
            let accepted = if cluster {
                let Some(cl) = &self.cluster_listener else { return Ok(()) };
                cl.accept()
            } else {
                let Some(l) = &self.listener else { return Ok(()) };
                l.accept()
            };
            match accepted {
                Ok(sock) => {
                    sock.set_nonblocking()?;
                    let _ = sock.set_nodelay();
                    let fd = sock.raw();
                    let id = self.next_conn_id;
                    self.next_conn_id += 1;
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

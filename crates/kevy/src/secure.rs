//! Keys for the encrypted links: generating one, loading one, and refusing
//! a configuration that turns a link on without what it needs. Nothing
//! here runs unless a link's `secure = true` is set.
//!
//! ```
//! let path = std::env::temp_dir().join(format!("kevy-doc-{}.key", std::process::id()));
//! let _ = std::fs::remove_file(&path);
//! let public = kevy::secure::keygen(&path).unwrap();
//! println!("peer_keys entry: n1={}", kevy_config::key_to_hex(&public));
//! std::fs::remove_file(&path).unwrap();
//! ```

use std::io::{self, Write as _};
use std::path::Path;

use kevy_config::{Config, ReplicationRole, key_from_hex, key_to_hex};
use kevy_noise::Keypair;

/// Write a new private key to `path` (created, never overwritten, mode
/// 0600) and return its public key.
///
/// ```
/// let path = std::env::temp_dir().join(format!("kevy-keygen-{}.key", std::process::id()));
/// let _ = std::fs::remove_file(&path);
/// kevy::secure::keygen(&path).unwrap();
/// assert!(kevy::secure::keygen(&path).is_err(), "never overwrites a key");
/// std::fs::remove_file(&path).unwrap();
/// ```
pub fn keygen(path: impl AsRef<Path>) -> io::Result<[u8; 32]> {
    let mut secret = [0u8; 32];
    kevy_sys::fill_random(&mut secret)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts.open(path)?;
    writeln!(f, "{}", key_to_hex(&secret))?;
    f.sync_all()?;
    Ok(Keypair::from_secret(secret).public())
}

/// Read a private key written by [`keygen`]. A file others can read is
/// refused rather than used.
pub(crate) fn load_keypair(path: &Path) -> Result<Keypair, String> {
    let shown = path.display();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(|e| format!("kevy: private key {shown}: {e}"))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "kevy: private key {shown} is readable by others (mode {:o}); chmod 600 it",
                mode & 0o777
            ));
        }
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("kevy: private key {shown}: {e}"))?;
    let secret = key_from_hex(&text).map_err(|e| format!("kevy: private key {shown}: {e}"))?;
    Ok(Keypair::from_secret(secret))
}

/// This node's key when any link is secure, after checking that every
/// secure link has the keys it will need. `Ok(None)`: nothing is secure.
pub(crate) fn link_keypair(cfg: &Config) -> Result<Option<Keypair>, String> {
    let (cluster, repl) = (cfg.cluster.secure, cfg.replication.secure);
    let clients = cfg.secure.listen_port != 0;
    if !cluster && !repl && !clients {
        return Ok(None);
    }
    let path = cfg.secure.private_key_file.as_deref().ok_or_else(|| {
        "kevy: a link is secure but [secure] private_key_file is not set (create one with `kevy keygen <file>`)"
            .to_string()
    })?;
    if clients {
        check_client_ports(cfg)?;
    }
    if cluster {
        for p in cfg.cluster.peers.iter().filter(|p| p.node_id != cfg.cluster.node_id) {
            if !cfg.cluster.peer_keys.iter().any(|(id, _)| *id == p.node_id) {
                return Err(format!(
                    "kevy: [cluster] secure, but peer_keys has no key for peer {:?}",
                    p.node_id
                ));
            }
        }
    }
    if repl
        && cfg.replication.role == ReplicationRole::Replica
        && cfg.replication.upstream_key.is_none()
    {
        return Err("kevy: [replication] secure on a replica needs upstream_key".to_string());
    }
    load_keypair(path).map(Some)
}

/// The encrypted ports must not collide with each other or with the
/// plaintext ports, cluster ranges included.
fn check_client_ports(cfg: &Config) -> Result<(), String> {
    let n = if cfg.cluster.enabled { u32::from(cfg.server.threads.max(1) as u16) } else { 0 };
    let mut taken: Vec<(u32, u32, &str)> = vec![(u32::from(cfg.server.port), 1, "port")];
    if n > 0 {
        taken.push((u32::from(crate::cluster_port_base(cfg)), n, "the cluster ports"));
    }
    let mut ours: Vec<(u32, u32, &str)> =
        vec![(u32::from(cfg.secure.listen_port), 1, "[secure] listen_port")];
    if n > 0 {
        let base = u32::from(crate::secure_front::secure_cluster_port_base(cfg));
        if base + n > 65536 {
            return Err(format!("kevy: the encrypted cluster ports from {base} run past 65535"));
        }
        ours.push((base, n, "the encrypted cluster ports"));
    }
    let overlaps =
        |(a, al, _): (u32, u32, &str), (b, bl, _): (u32, u32, &str)| a < b + bl && b < a + al;
    for (i, &x) in ours.iter().enumerate() {
        for &y in taken.iter().chain(&ours[i + 1..]) {
            if overlaps(x, y) {
                return Err(format!("kevy: {} overlaps {}", x.2, y.2));
            }
        }
    }
    Ok(())
}

/// The keys a secure replication link needs on this node: its own, the
/// primaries it will follow, and the replicas it will serve.
pub(crate) struct ReplLinks {
    pub(crate) local: Keypair,
    /// `upstream_key` first, then every election peer's key: whichever
    /// node becomes primary, its key is here.
    primaries: Vec<[u8; 32]>,
    pub(crate) replicas: Vec<[u8; 32]>,
    last_good: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for ReplLinks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplLinks")
            .field("primaries", &self.primaries.len())
            .finish_non_exhaustive()
    }
}

impl ReplLinks {
    pub(crate) fn from_config(cfg: &Config, local: &Keypair) -> Self {
        let mut primaries: Vec<[u8; 32]> = cfg.replication.upstream_key.into_iter().collect();
        for (_, k) in &cfg.cluster.peer_keys {
            if !primaries.contains(k) && *k != local.public() {
                primaries.push(*k);
            }
        }
        ReplLinks {
            local: local.clone(),
            primaries,
            replicas: cfg.replication.replica_keys.clone(),
            last_good: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Open a secure link to `addr`, trying the key that worked last time
    /// first. A primary holding none of the trusted keys is never reached.
    pub(crate) fn connect(
        &self,
        addr: (std::net::IpAddr, u16),
        replica_id: &str,
        generation: u64,
        from_offset: u64,
    ) -> Result<kevy_replicate::replica::ReplicaClient, kevy_replicate::replica::ReplicaError> {
        use std::sync::atomic::Ordering;
        let n = self.primaries.len();
        let first = self.last_good.load(Ordering::Relaxed);
        let mut last = kevy_replicate::replica::ReplicaError::HandshakeRejected;
        for i in 0..n {
            let idx = (first + i) % n;
            let sec = kevy_replicate::replica::ReplicaSecurity::new(
                self.local.clone(),
                self.primaries[idx],
            );
            match kevy_replicate::replica::ReplicaClient::connect_with(
                addr,
                &kevy_replicate::replica::ConnectOptions::new(replica_id)
                    .with_from(kevy_replicate::feed::FeedPosition::new(generation, from_offset))
                    .with_security(sec),
            ) {
                Ok(c) => {
                    self.last_good.store(idx, Ordering::Relaxed);
                    return Ok(c);
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("kevy-secure-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn keygen_writes_a_private_file_once_and_it_loads_back() {
        let d = dir("keygen");
        let path = d.join("node.key");
        let public = keygen(&path).unwrap();
        assert_eq!(load_keypair(&path).unwrap().public(), public);
        assert!(keygen(&path).is_err(), "an existing key must not be overwritten");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(load_keypair(&path).unwrap_err().contains("readable by others"));
        }
    }

    // an embedded writer is a secure replication primary on a port the OS
    // picks, so no port has to be reserved ahead of time
    fn secure_primary(local: Keypair) -> (kevy_embedded::Store, u16) {
        let store = kevy_embedded::Store::open(
            kevy_embedded::Config::default()
                .with_embed_writer("127.0.0.1:0")
                .with_writer_security(kevy_embedded::LinkKeys::new(local)),
        )
        .unwrap();
        let port = store.writer_addr().unwrap().port();
        (store, port)
    }

    #[test]
    fn repl_links_try_each_trusted_primary_and_remember_the_one_that_answered() {
        let primary = Keypair::from_secret([5; 32]);
        let (_primary, repl) = secure_primary(primary.clone());
        let me = Keypair::from_secret([6; 32]);
        let mut cfg = Config::default();
        cfg.replication.upstream_key = Some([9; 32]); // a stale key first
        cfg.cluster.peer_keys = vec![("p".into(), primary.public())];
        let links = ReplLinks::from_config(&cfg, &me);
        let addr = (std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), repl);
        assert!(links.connect(addr, "r", 0, 0).is_ok(), "the second trusted key is the primary's");
        assert_eq!(links.last_good.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(links.connect(addr, "r", 0, 0).is_ok(), "and it is tried first next time");

        cfg.cluster.peer_keys.clear();
        let strangers = ReplLinks::from_config(&cfg, &me);
        assert!(strangers.connect(addr, "r", 0, 0).is_err(), "no trusted key, no link");
        let shown = format!("{strangers:?}");
        assert!(shown.contains("primaries: 1"), "{shown}");
        assert!(!shown.contains(&format!("{:?}", [6u8; 32])), "{shown}");
    }

    #[test]
    fn a_secure_link_without_its_keys_refuses_to_start() {
        let mut cfg = Config::default();
        assert_eq!(link_keypair(&cfg).map(|k| k.is_some()), Ok(false));
        cfg.cluster.secure = true;
        assert!(link_keypair(&cfg).unwrap_err().contains("private_key_file"));
        let d = dir("refuse");
        let path = d.join("node.key");
        keygen(&path).unwrap();
        cfg.secure.private_key_file = Some(path);
        cfg.cluster.node_id = "n1".into();
        cfg.cluster.peers = kevy_config::PeerEntry::parse_list("n1@h:1,n2@h:2").unwrap();
        assert!(link_keypair(&cfg).unwrap_err().contains("\"n2\""));
        cfg.cluster.peer_keys = vec![("n2".into(), [7; 32])];
        assert!(link_keypair(&cfg).unwrap().is_some());
        cfg.replication.secure = true;
        cfg.replication.role = ReplicationRole::Replica;
        assert!(link_keypair(&cfg).unwrap_err().contains("upstream_key"));

        let mut clients = Config::default();
        clients.secure.listen_port = clients.server.port;
        clients.secure.private_key_file = cfg.secure.private_key_file.clone();
        assert!(link_keypair(&clients).unwrap_err().contains("overlaps port"));
        clients.secure.listen_port = clients.server.port + 400;
        assert!(link_keypair(&clients).unwrap().is_some(), "the client port alone needs the key");
        // in cluster mode the plaintext cluster ports start at port + 1
        clients.cluster.enabled = true;
        clients.server.threads = 4;
        clients.secure.listen_port = clients.server.port + 2;
        assert!(link_keypair(&clients).unwrap_err().contains("overlaps the cluster ports"));
        clients.secure.listen_port = clients.server.port + 400;
        clients.secure.cluster_port_base = 65534;
        assert!(link_keypair(&clients).unwrap_err().contains("past 65535"));
        clients.secure.cluster_port_base = 0; // listen_port + 1 ..= + 4
        assert!(link_keypair(&clients).unwrap().is_some());
    }
}

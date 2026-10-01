//! TCP control-plane transport for [`crate::Elector`] — the network
//! half. Drives the elector by reading inbound frames off
//! one accept-side listener + writing outbound frames over one
//! persistent connection per peer.
//!
//! Architecture: **one thread for the listener** + **one thread per
//! outbound peer** + **one orchestrator thread** that owns the
//! `Elector` and drives `tick` / `on_message` against it. Inbound
//! frames + outbound dispatch + tick fire all flow through MPSC
//! channels into the orchestrator (single-threaded against the
//! elector — no Mutex on the hot path).
//!
//! Sockets are blocking TCP — kevy-elect's traffic is rare
//! (heartbeats at 5 Hz default) so the busy-wait / async machinery
//! that the keyspace plane needs is overkill here. The orchestrator
//! checks the inbound channel with `recv_timeout(hb_interval)` so
//! ticks fire at the configured cadence without burning a core.
//!
//! Out of scope (Phase 1.5): TLS / auth / connection pooling.
//!
//! ```
//! # use std::net::{IpAddr, Ipv4Addr}; use std::time::{Duration, Instant};
//! # use kevy_elect::{ElectConfig, Elector, PeerAddr, Role, Transport};
//! # const LOCAL: (IpAddr, u16) = (IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
//! # fn node(id: &str, peers: &[&str], role: Role) -> Elector { let cfg = ElectConfig::default().with_hb_interval(Duration::from_millis(20)).with_down_after(Duration::from_millis(200)); Elector::new(id, peers.iter().map(|p| p.to_string()).collect(), "x:6004", role).with_config(cfg) }
//! # use kevy_elect::ElectorSnapshot; fn wait(t: &Transport, done: impl Fn(&ElectorSnapshot) -> bool) -> ElectorSnapshot { let end = Instant::now() + Duration::from_secs(20); loop { let s = t.state_snapshot(); if done(&s) || Instant::now() > end { return s; } std::thread::sleep(Duration::from_millis(10)); } }
//! // a one-node cluster: after the grace window it elects itself
//! let t = Transport::spawn(node("a", &["a"], Role::Replica), LOCAL, vec![])?;
//! let snap = wait(&t, |s| s.role == Role::Primary);
//! assert_eq!(snap.current_primary.as_deref(), Some("a"));
//! t.shutdown();
//! # Ok::<(), std::io::Error>(())
//! ```

// Teardown. `join` returns what the thread panicked with and the
// thread is already being abandoned; a flush on the way out has
// nowhere left to put its bytes. No caller remains to be told.
#![expect(clippy::let_underscore_must_use, reason = "teardown has nobody left to report to")]

use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::elector::Elector;
use crate::transport_loops::{
    InboundEvent, Shared, orchestrator_loop, spawn_listener_thread, spawn_outbound_threads,
};

/// Topology-change callback:
/// `(new_local_role, Some(primary_id) when known, has_quorum)`.
/// Address mapping is the CALLER's job (the static member table
/// lives in the host's config — membership is static, roles are
/// dynamic). `has_quorum` drives the primary lease: a primary seeing
/// `false` is on the minority side of a partition and must fence
/// writes within the `down_after` window.
///
/// ```
/// use std::sync::mpsc::channel;
/// use kevy_elect::{Role, TopologyCallback};
///
/// let (tx, rx) = channel();
/// let on_change: TopologyCallback = Box::new(move |role, primary, has_quorum| {
///     let fence = role == Role::Primary && !has_quorum; // stop taking writes
///     let _ = tx.send((primary, fence));
/// });
/// on_change(Role::Primary, Some("a".to_string()), false);
/// assert_eq!(rx.recv()?, (Some("a".to_string()), true));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub type TopologyCallback = Box<dyn Fn(crate::message::Role, Option<String>, bool) + Send>;

/// Per-peer addressing. Maps `node_id` → outbound dial address.
///
/// ```
/// let peer = kevy_elect::PeerAddr::new("n2", "10.0.0.2", 7004);
/// assert_eq!((peer.node_id.as_str(), peer.port), ("n2", 7004));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PeerAddr {
    /// Peer's stable node id (matches the `node_id` field the
    /// peer puts in its `HB`).
    ///
    /// ```
    /// let peer = kevy_elect::PeerAddr::new("n2", "10.0.0.2", 7004);
    /// assert_eq!(peer.node_id, "n2"); // the id n2 puts in its heartbeats
    /// ```
    pub node_id: String,
    /// Peer's elect-control host (IP or DNS).
    ///
    /// ```
    /// let peer = kevy_elect::PeerAddr::new("n2", "db2.internal", 7004);
    /// assert_eq!(format!("{}:{}", peer.host, peer.port), "db2.internal:7004"); // what gets dialled
    /// ```
    pub host: String,
    /// Peer's elect-control TCP port.
    ///
    /// ```
    /// use std::net::TcpListener;
    ///
    /// let listener = TcpListener::bind("127.0.0.1:0")?;
    /// let peer = kevy_elect::PeerAddr::new("n2", "127.0.0.1", listener.local_addr()?.port());
    /// assert_eq!(peer.port, listener.local_addr()?.port());
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub port: u16,
}

impl PeerAddr {
    /// The dial address of peer `node_id`.
    ///
    /// ```
    /// let peer = kevy_elect::PeerAddr::new("n3", "db3.internal", 7004);
    /// assert_eq!(peer.host, "db3.internal");
    /// ```
    pub fn new(node_id: impl Into<String>, host: impl Into<String>, port: u16) -> Self {
        Self { node_id: node_id.into(), host: host.into(), port }
    }
}

/// Public handle to a running transport. Owns the orchestrator +
/// listener + outbound worker threads. Dropping it signals stop
/// and joins (best-effort within `JOIN_TIMEOUT`).
///
/// Two nodes on loopback; the replica learns the primary from its
/// heartbeats:
///
/// ```
/// # use std::net::{IpAddr, Ipv4Addr}; use std::time::{Duration, Instant};
/// # use kevy_elect::{ElectConfig, Elector, PeerAddr, Role, Transport};
/// # const LOCAL: (IpAddr, u16) = (IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
/// # fn node(id: &str, peers: &[&str], role: Role) -> Elector { let cfg = ElectConfig::default().with_hb_interval(Duration::from_millis(20)).with_down_after(Duration::from_millis(200)); Elector::new(id, peers.iter().map(|p| p.to_string()).collect(), "x:6004", role).with_config(cfg) }
/// # use kevy_elect::ElectorSnapshot; fn wait(t: &Transport, done: impl Fn(&ElectorSnapshot) -> bool) -> ElectorSnapshot { let end = Instant::now() + Duration::from_secs(20); loop { let s = t.state_snapshot(); if done(&s) || Instant::now() > end { return s; } std::thread::sleep(Duration::from_millis(10)); } }
/// let ports = kevy_testnet::free_ports(2);
/// let at = |id: &str, port| PeerAddr::new(id, "127.0.0.1", port);
/// let a = Transport::spawn(node("a", &["a", "b"], Role::Primary), (LOCAL.0, ports[0]), vec![at("b", ports[1])])?;
/// let b = Transport::spawn(node("b", &["a", "b"], Role::Replica), (LOCAL.0, ports[1]), vec![at("a", ports[0])])?;
/// let snap = wait(&b, |s| s.current_primary.is_some());
/// assert_eq!((snap.role, snap.current_primary.as_deref()), (Role::Replica, Some("a")));
/// assert!(wait(&b, |s| s.down_peers.is_empty()).down_peers.is_empty()); // a is heard
/// a.shutdown();
/// b.shutdown();
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug)]
pub struct Transport {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    shared: Arc<Shared>,
    /// Cloned at construction-time so the kevy-server adapter can
    /// query the live `epoch` / `role` / `current_primary` without
    /// owning the inbound channel.
    state_view: Arc<Shared>,
}

impl Transport {
    /// Spawn the listener, per-peer outbound workers, and the
    /// orchestrator. Returns immediately — the threads run until
    /// `Transport` is dropped.
    ///
    /// `listen_addr` is the local `host:port` the listener binds
    /// to (typically `0.0.0.0:elect_port`). `peers` lists every
    /// OTHER node in the cluster (this node's own id is filtered
    /// out by the elector at run-time).
    ///
    /// ```
    /// # use std::net::{IpAddr, Ipv4Addr}; use std::time::{Duration, Instant};
    /// # use kevy_elect::{ElectConfig, Elector, PeerAddr, Role, Transport};
    /// # const LOCAL: (IpAddr, u16) = (IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    /// # fn node(id: &str, peers: &[&str], role: Role) -> Elector { let cfg = ElectConfig::default().with_hb_interval(Duration::from_millis(20)).with_down_after(Duration::from_millis(200)); Elector::new(id, peers.iter().map(|p| p.to_string()).collect(), "x:6004", role).with_config(cfg) }
    /// let t = Transport::spawn(node("a", &["a"], Role::Primary), LOCAL, vec![])?;
    /// assert_eq!(t.state_snapshot().role, Role::Primary);
    /// t.shutdown();
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn spawn(
        elector: Elector,
        listen_addr: (std::net::IpAddr, u16),
        peers: Vec<PeerAddr>,
    ) -> std::io::Result<Self> {
        Self::spawn_with_callback(elector, listen_addr, peers, Box::new(|_, _, _| {}))
    }

    /// Like [`Self::spawn`], with a topology-change
    /// callback: fired from the orchestrator thread whenever
    /// `(role, current_primary)` changes after a message or tick.
    /// Arguments: the new local role, and `Some((primary_id,
    /// primary_addr))` when a primary is known. The callback MUST be
    /// quick and non-reentrant into the elector (it runs outside the
    /// elector lock but on the tick thread).
    ///
    /// ```
    /// # use std::net::{IpAddr, Ipv4Addr}; use std::time::{Duration, Instant};
    /// # use kevy_elect::{ElectConfig, Elector, PeerAddr, Role, Transport};
    /// # const LOCAL: (IpAddr, u16) = (IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    /// # fn node(id: &str, peers: &[&str], role: Role) -> Elector { let cfg = ElectConfig::default().with_hb_interval(Duration::from_millis(20)).with_down_after(Duration::from_millis(200)); Elector::new(id, peers.iter().map(|p| p.to_string()).collect(), "x:6004", role).with_config(cfg) }
    /// use std::sync::mpsc::channel;
    ///
    /// let (tx, rx) = channel();
    /// let t = Transport::spawn_with_callback(node("a", &["a"], Role::Replica), LOCAL, vec![],
    ///     Box::new(move |role, primary, _quorum| { let _ = tx.send((role, primary)); }))?;
    /// assert_eq!(rx.recv_timeout(Duration::from_secs(20))?, (Role::Replica, None)); // the start
    /// // the grace window passes and the node elects itself
    /// assert_eq!(rx.recv_timeout(Duration::from_secs(20))?, (Role::Primary, Some("a".to_string())));
    /// t.shutdown();
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    // needless_pass_by_value: `peers` is handed to the spawned outbound loops
    // one entry at a time; by-value keeps the pub API an ownership handoff.
    #[allow(clippy::needless_pass_by_value)]
    pub fn spawn_with_callback(
        elector: Elector,
        listen_addr: (std::net::IpAddr, u16),
        peers: Vec<PeerAddr>,
        on_change: TopologyCallback,
    ) -> std::io::Result<Self> {
        Self::spawn_inner(elector, listen_addr, peers, on_change, None)
    }

    /// Like [`Self::spawn_with_callback`], with every link encrypted and
    /// both ends authenticated by their keys (Noise IK). A connection from
    /// a key not in `secure.peer_keys` is dropped before anything it sends
    /// is read, and a message claiming a sender other than the key's node
    /// closes the link.
    ///
    /// ```
    /// use std::net::{IpAddr, Ipv4Addr};
    /// use std::time::Duration;
    /// use kevy_elect::{ElectConfig, ElectJitter, Elector, Role, SecureLinks, Transport};
    /// use kevy_noise::Keypair;
    ///
    /// let elector = Elector::new("a", vec!["a".to_string()], "127.0.0.1:0", Role::Primary)
    ///     .with_config(ElectConfig::default().with_hb_interval(Duration::from_millis(50)))
    ///     .with_jitter(ElectJitter::Fixed(Duration::ZERO));
    /// let secure = SecureLinks::new(Keypair::from_secret([1; 32]), []);
    /// let t = Transport::spawn_secure(elector, (IpAddr::V4(Ipv4Addr::LOCALHOST), 0), vec![],
    ///     Box::new(|_, _, _| {}), secure)?;
    /// assert_eq!(t.state_snapshot().role, Role::Primary);
    /// t.shutdown();
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn spawn_secure(
        elector: Elector,
        listen_addr: (std::net::IpAddr, u16),
        peers: Vec<PeerAddr>,
        on_change: TopologyCallback,
        secure: crate::link::SecureLinks,
    ) -> std::io::Result<Self> {
        Self::spawn_inner(elector, listen_addr, peers, on_change, Some(secure))
    }

    fn spawn_inner(
        elector: Elector,
        listen_addr: (std::net::IpAddr, u16),
        peers: Vec<PeerAddr>,
        on_change: TopologyCallback,
        secure: Option<crate::link::SecureLinks>,
    ) -> std::io::Result<Self> {
        let hb_interval = elector.config.hb_interval;
        let shared = Arc::new(Shared {
            elector: Mutex::new(elector),
            secure,
            out_queues: Mutex::new(std::collections::HashMap::new()),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        let (inbound_tx, inbound_rx) = channel::<InboundEvent>();

        let listener = TcpListener::bind(listen_addr)?;
        listener.set_nonblocking(false)?;
        spawn_listener_thread(listener, inbound_tx.clone(), stop.clone(), &shared, &mut handles)?;
        spawn_outbound_threads(&peers, &shared, &stop, &mut handles)?;

        let orch_stop = stop.clone();
        let orch_shared = shared.clone();
        handles.push(
            std::thread::Builder::new().name("kevy-elect-orchestrator".to_string()).spawn(
                move || {
                    orchestrator_loop(orch_shared, inbound_rx, hb_interval, orch_stop, on_change);
                },
            )?,
        );

        Ok(Self { stop, handles, state_view: shared.clone(), shared })
    }

    /// Read-side snapshot of the elector for `ROLE` / `INFO
    /// replication`. Locks the elector mutex briefly; cheap.
    ///
    /// ```
    /// # use std::net::{IpAddr, Ipv4Addr}; use std::time::{Duration, Instant};
    /// # use kevy_elect::{ElectConfig, Elector, PeerAddr, Role, Transport};
    /// # const LOCAL: (IpAddr, u16) = (IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    /// # fn node(id: &str, peers: &[&str], role: Role) -> Elector { let cfg = ElectConfig::default().with_hb_interval(Duration::from_millis(20)).with_down_after(Duration::from_millis(200)); Elector::new(id, peers.iter().map(|p| p.to_string()).collect(), "x:6004", role).with_config(cfg) }
    /// let t = Transport::spawn(node("a", &["a", "b"], Role::Primary), LOCAL, vec![])?;
    /// let snap = t.state_snapshot();
    /// assert_eq!((snap.role, snap.epoch, snap.current_primary), (Role::Primary, 1, None));
    /// assert_eq!(snap.down_peers, ["b"]); // never heard from
    /// t.shutdown();
    /// # Ok::<(), std::io::Error>(())
    /// ```
    // missing_panics_doc: lock().expect — poisoning means another thread
    // already panicked mid-election; propagating is the only sane behaviour.
    #[allow(clippy::missing_panics_doc)]
    pub fn state_snapshot(&self) -> ElectorSnapshot {
        let e = self.state_view.elector.lock().expect("elector lock");
        let now = std::time::Instant::now();
        // Include the list of peers this node considers
        // DOWN at snapshot time. kevy-scope's fallback path reads
        // this to decide "writer DOWN → fallback takes over"; the
        // computation here is cheap (one pass over peer_ids).
        let down_peers: Vec<String> = e
            .peer_ids
            .iter()
            .filter(|id| id.as_str() != e.node_id.as_str())
            .filter(|id| e.is_peer_down(id, now))
            .cloned()
            .collect();
        ElectorSnapshot {
            role: e.role(),
            epoch: e.epoch(),
            current_primary: e.current_primary().map(str::to_string),
            down_peers,
        }
    }

    /// Feed this node's replication offset into the elector.
    ///
    /// ```
    /// # use std::net::{IpAddr, Ipv4Addr}; use std::time::{Duration, Instant};
    /// # use kevy_elect::{ElectConfig, Elector, PeerAddr, Role, Transport};
    /// # const LOCAL: (IpAddr, u16) = (IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    /// # fn node(id: &str, peers: &[&str], role: Role) -> Elector { let cfg = ElectConfig::default().with_hb_interval(Duration::from_millis(20)).with_down_after(Duration::from_millis(200)); Elector::new(id, peers.iter().map(|p| p.to_string()).collect(), "x:6004", role).with_config(cfg) }
    /// use std::io::Read;
    /// use std::net::TcpListener;
    /// use kevy_elect::Message;
    ///
    /// // stand in for peer b and read what a sends it
    /// let b = TcpListener::bind("127.0.0.1:0")?;
    /// let peers = vec![PeerAddr::new("b", "127.0.0.1", b.local_addr()?.port())];
    /// let t = Transport::spawn(node("a", &["a", "b"], Role::Primary), LOCAL, peers)?;
    /// t.set_repl_offset(4242);
    /// let (mut link, _) = b.accept()?;
    /// link.set_read_timeout(Some(Duration::from_secs(20)))?;
    /// let (mut buf, mut chunk, mut seen) = (Vec::new(), [0u8; 256], 0);
    /// while seen != 4242 {
    ///     let n = link.read(&mut chunk)?;
    ///     assert!(n > 0, "link closed");
    ///     buf.extend_from_slice(&chunk[..n]);
    ///     while let Ok((msg, used)) = Message::decode(&buf) {
    ///         buf.drain(..used);
    ///         if let Message::Hb { repl_offset, .. } = msg {
    ///             seen = repl_offset; // heartbeats now carry the offset
    ///         }
    ///     }
    /// }
    /// t.shutdown();
    /// # Ok::<(), std::io::Error>(())
    /// ```
    // missing_panics_doc: same poisoned-lock rationale as `state_snapshot`.
    #[allow(clippy::missing_panics_doc)]
    pub fn set_repl_offset(&self, offset: u64) {
        self.shared.elector.lock().expect("elector lock").set_repl_offset(offset);
    }

    /// Stop the transport. Joins all threads (with best-effort
    /// timeout). Idempotent.
    ///
    /// ```
    /// # use std::net::{IpAddr, Ipv4Addr}; use std::time::{Duration, Instant};
    /// # use kevy_elect::{ElectConfig, Elector, PeerAddr, Role, Transport};
    /// # const LOCAL: (IpAddr, u16) = (IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    /// # fn node(id: &str, peers: &[&str], role: Role) -> Elector { let cfg = ElectConfig::default().with_hb_interval(Duration::from_millis(20)).with_down_after(Duration::from_millis(200)); Elector::new(id, peers.iter().map(|p| p.to_string()).collect(), "x:6004", role).with_config(cfg) }
    /// use std::net::TcpListener;
    ///
    /// let port = kevy_testnet::free_port();
    /// let t = Transport::spawn(node("a", &["a"], Role::Primary), (LOCAL.0, port), vec![])?;
    /// t.shutdown();
    /// // every thread has exited, so the listening port is free again
    /// drop(TcpListener::bind(("127.0.0.1", port))?);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Drain handles. We can't tell threads to exit a blocking
        // recv mid-flight (channel close on Sender drop handles it),
        // but the per-loop checks of `stop` flag are the canonical
        // exit signal.
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Read-side snapshot returned by [`Transport::state_snapshot`].
///
/// ```
/// # use std::net::{IpAddr, Ipv4Addr}; use std::time::{Duration, Instant};
/// # use kevy_elect::{ElectConfig, Elector, PeerAddr, Role, Transport};
/// # const LOCAL: (IpAddr, u16) = (IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
/// # fn node(id: &str, peers: &[&str], role: Role) -> Elector { let cfg = ElectConfig::default().with_hb_interval(Duration::from_millis(20)).with_down_after(Duration::from_millis(200)); Elector::new(id, peers.iter().map(|p| p.to_string()).collect(), "x:6004", role).with_config(cfg) }
/// # use kevy_elect::ElectorSnapshot; fn wait(t: &Transport, done: impl Fn(&ElectorSnapshot) -> bool) -> ElectorSnapshot { let end = Instant::now() + Duration::from_secs(20); loop { let s = t.state_snapshot(); if done(&s) || Instant::now() > end { return s; } std::thread::sleep(Duration::from_millis(10)); } }
/// let t = Transport::spawn(node("a", &["a"], Role::Replica), LOCAL, vec![])?;
/// let ElectorSnapshot { role, epoch, current_primary, down_peers, .. } = wait(&t, |s| s.role == Role::Primary);
/// assert_eq!((role, epoch, current_primary.as_deref()), (Role::Primary, 2, Some("a")));
/// assert!(down_peers.is_empty());
/// t.shutdown();
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ElectorSnapshot {
    /// Self-perceived role at snapshot time.
    ///
    /// ```
    /// # use std::net::{IpAddr, Ipv4Addr}; use std::time::{Duration, Instant};
    /// # use kevy_elect::{ElectConfig, Elector, PeerAddr, Role, Transport};
    /// # const LOCAL: (IpAddr, u16) = (IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    /// # fn node(id: &str, peers: &[&str], role: Role) -> Elector { let cfg = ElectConfig::default().with_hb_interval(Duration::from_millis(20)).with_down_after(Duration::from_millis(200)); Elector::new(id, peers.iter().map(|p| p.to_string()).collect(), "x:6004", role).with_config(cfg) }
    /// let t = Transport::spawn(node("a", &["a", "b"], Role::Replica), LOCAL, vec![])?;
    /// assert_eq!(t.state_snapshot().role, Role::Replica); // until an election says otherwise
    /// t.shutdown();
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub role: crate::message::Role,
    /// Election epoch at snapshot time.
    ///
    /// ```
    /// # use std::net::{IpAddr, Ipv4Addr}; use std::time::{Duration, Instant};
    /// # use kevy_elect::{ElectConfig, Elector, PeerAddr, Role, Transport};
    /// # const LOCAL: (IpAddr, u16) = (IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    /// # fn node(id: &str, peers: &[&str], role: Role) -> Elector { let cfg = ElectConfig::default().with_hb_interval(Duration::from_millis(20)).with_down_after(Duration::from_millis(200)); Elector::new(id, peers.iter().map(|p| p.to_string()).collect(), "x:6004", role).with_config(cfg) }
    /// # use kevy_elect::ElectorSnapshot; fn wait(t: &Transport, done: impl Fn(&ElectorSnapshot) -> bool) -> ElectorSnapshot { let end = Instant::now() + Duration::from_secs(20); loop { let s = t.state_snapshot(); if done(&s) || Instant::now() > end { return s; } std::thread::sleep(Duration::from_millis(10)); } }
    /// let t = Transport::spawn(node("a", &["a"], Role::Replica), LOCAL, vec![])?;
    /// assert_eq!(t.state_snapshot().epoch, 1);
    /// assert_eq!(wait(&t, |s| s.role == Role::Primary).epoch, 2); // winning took a new epoch
    /// t.shutdown();
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub epoch: u64,
    /// Currently-known primary id (`None` until first ANNOUNCE).
    ///
    /// ```
    /// # use std::net::{IpAddr, Ipv4Addr}; use std::time::{Duration, Instant};
    /// # use kevy_elect::{ElectConfig, Elector, PeerAddr, Role, Transport};
    /// # const LOCAL: (IpAddr, u16) = (IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    /// # fn node(id: &str, peers: &[&str], role: Role) -> Elector { let cfg = ElectConfig::default().with_hb_interval(Duration::from_millis(20)).with_down_after(Duration::from_millis(200)); Elector::new(id, peers.iter().map(|p| p.to_string()).collect(), "x:6004", role).with_config(cfg) }
    /// # use kevy_elect::ElectorSnapshot; fn wait(t: &Transport, done: impl Fn(&ElectorSnapshot) -> bool) -> ElectorSnapshot { let end = Instant::now() + Duration::from_secs(20); loop { let s = t.state_snapshot(); if done(&s) || Instant::now() > end { return s; } std::thread::sleep(Duration::from_millis(10)); } }
    /// let t = Transport::spawn(node("a", &["a"], Role::Replica), LOCAL, vec![])?;
    /// assert_eq!(t.state_snapshot().current_primary, None);
    /// let snap = wait(&t, |s| s.current_primary.is_some());
    /// assert_eq!(snap.current_primary.as_deref(), Some("a"));
    /// t.shutdown();
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub current_primary: Option<String>,
    /// Peers (excluding self) whose last `HB` is older than
    /// `ElectConfig::down_after` — i.e. the down-set this node would
    /// vote on at quorum time. kevy-scope's F4 fallback reads this
    /// to decide whether the declared scope writer is reachable;
    /// when the writer's id is present, the fallback takes over the
    /// scope's writes.
    ///
    /// ```
    /// # use std::net::{IpAddr, Ipv4Addr}; use std::time::{Duration, Instant};
    /// # use kevy_elect::{ElectConfig, Elector, PeerAddr, Role, Transport};
    /// # const LOCAL: (IpAddr, u16) = (IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    /// # fn node(id: &str, peers: &[&str], role: Role) -> Elector { let cfg = ElectConfig::default().with_hb_interval(Duration::from_millis(20)).with_down_after(Duration::from_millis(200)); Elector::new(id, peers.iter().map(|p| p.to_string()).collect(), "x:6004", role).with_config(cfg) }
    /// // b and c are declared but no link to either is configured
    /// let t = Transport::spawn(node("a", &["a", "b", "c"], Role::Primary), LOCAL, vec![])?;
    /// let snap = t.state_snapshot();
    /// let scope_writer = "c";
    /// assert!(snap.down_peers.iter().any(|p| p == scope_writer)); // a fallback would take over
    /// assert_eq!(snap.down_peers, ["b", "c"]);
    /// t.shutdown();
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub down_peers: Vec<String>,
}

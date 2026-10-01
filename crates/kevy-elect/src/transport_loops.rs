//! Per-thread loop bodies for [`crate::transport::Transport`] —
//! pulled out of `transport.rs` so that file stays under the
//! project's 500-LOC ceiling. The listener / per-peer outbound /
//! orchestrator threads spawned by `Transport::spawn_with_callback`
//! run these functions, and the helpers that spawn the listener and
//! outbound threads live here with them; the handle type and shared
//! state stay in `transport.rs`.

// Socket options are advisory here. `set_nodelay`, `set_read_timeout`
// and `set_nonblocking` shape latency, not correctness — a kernel that
// declines one leaves a connection that still elects, just less
// promptly — and a thread that will not spawn is reported by the
// election timing out, which is the signal this module already watches.
#![expect(clippy::let_underscore_must_use, reason = "socket tuning is advisory to an election")]

use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::elector::{Elector, Outbound};
use crate::link::{Link, initiate, respond};
use crate::message::Message;
use crate::transport::{PeerAddr, TopologyCallback};
use crate::wire::DecodeError;

/// Maximum buffer the per-connection reader holds before declaring
/// the framing busted. Election frames are ≤ 256 B; 16 KiB is
/// generous for misaligned partial reads.
pub(crate) const READ_BUF_CAP: usize = 16 * 1024;

/// Read-loop sleep on transient EAGAIN-equivalents (peer closed,
/// I/O error during decode). Keeps the worker from a tight retry
/// loop while still recovering on reconnect.
pub(crate) const READ_RETRY_BACKOFF: Duration = Duration::from_millis(100);

/// One inbound event the orchestrator processes. Either a decoded
/// election message from a peer, or a "the connection from $peer
/// went down" notification (so the orchestrator can clear any
/// state that assumed the link was up).
#[derive(Debug)]
pub(crate) enum InboundEvent {
    /// `(from_node_id, msg)`.
    Message(String, Message),
    /// An inbound connection failed its handshake, closed, or sent a
    /// frame that does not decode.
    InboundConnFailed,
}

/// Shared state between the orchestrator + worker threads. Wraps
/// the elector in a Mutex so the per-peer outbound threads can read
/// the latest `epoch` / `repl_offset` for the next heartbeat
/// without round-tripping through the orchestrator — but **only the
/// orchestrator mutates** via `tick` / `on_message`.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) elector: Mutex<Elector>,
    /// `Some` when every election link must be Noise-authenticated.
    pub(crate) secure: Option<crate::link::SecureLinks>,
    /// Per-peer outbound queue. Indexed by `node_id`. Each worker
    /// drains its own queue + writes onto the persistent TCP
    /// stream; on stream death the queue is held until the worker
    /// reconnects. Bounded by `MAX_PENDING_PER_PEER` to prevent a
    /// dead peer from leaking memory.
    pub(crate) out_queues:
        Mutex<std::collections::HashMap<String, std::collections::VecDeque<Message>>>,
}

pub(crate) const MAX_PENDING_PER_PEER: usize = 256;

// needless_pass_by_value: thread entry point — it owns its channel/flag for
// the thread's whole lifetime; references cannot cross `thread::spawn`.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn accept_loop(
    listener: TcpListener,
    tx: Sender<InboundEvent>,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
) {
    // Non-blocking + short sleep so the loop can observe `stop`
    // between accepts. Blocking `accept` would need a Shutdown-on-
    // try_clone trick to interrupt; the non-blocking poll keeps the
    // surface uniform with the outbound loop's busy-but-cheap
    // pattern (election control plane is low-volume).
    listener.set_nonblocking(true).expect("listener set_nonblocking(true)");
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, addr)) => {
                let _ = stream.set_nonblocking(false); // children block on reads.
                let tx_clone = tx.clone();
                let stop_clone = stop.clone();
                let shared = Arc::clone(&shared);
                let addr_str = addr.to_string();
                let _ = std::thread::Builder::new()
                    .name(format!("kevy-elect-in-{addr_str}"))
                    .spawn(move || {
                        // the handshake runs on this thread, never on the acceptor
                        let link = match &shared.secure {
                            None => Some((Link::Plain(stream), None)),
                            Some(secure) => {
                                respond(stream, secure).ok().map(|(l, id)| (l, Some(id)))
                            }
                        };
                        if let Some((link, verified)) = link {
                            inbound_read_loop(link, verified, tx_clone, stop_clone);
                        }
                    });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                std::thread::sleep(READ_RETRY_BACKOFF);
            }
        }
    }
}

// needless_pass_by_value: thread entry point (see `accept_loop`).
#[allow(clippy::needless_pass_by_value)]
fn inbound_read_loop(
    mut link: Link,
    verified: Option<String>,
    tx: Sender<InboundEvent>,
    stop: Arc<AtomicBool>,
) {
    let _ = link.stream().set_nodelay(true);
    // Short read timeout so the loop can observe `stop` between
    // reads. Blocking read otherwise can't be interrupted by a
    // flag.
    let _ = link.stream().set_read_timeout(Some(Duration::from_millis(200)));
    let mut buf: Vec<u8> = Vec::with_capacity(READ_BUF_CAP);
    let mut chunk = [0u8; 1024];
    while !stop.load(Ordering::Relaxed) {
        match link.read_into(&mut chunk, &mut buf) {
            Ok(0) => {
                let _ = tx.send(InboundEvent::InboundConnFailed);
                return;
            }
            Ok(_) => {
                if buf.len() > READ_BUF_CAP {
                    let _ = tx.send(InboundEvent::InboundConnFailed);
                    return;
                }
                if !drain_frames(&mut buf, &tx, verified.as_deref()) {
                    return;
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Read timeout — fall through to re-check `stop`.
            }
            Err(_) => {
                let _ = tx.send(InboundEvent::InboundConnFailed);
                return;
            }
        }
    }
}

/// Decode + dispatch every complete frame sitting in `buf`. Returns
/// `false` when the framing is busted — an `InboundConnFailed` has
/// been sent and the caller must drop the connection.
fn drain_frames(buf: &mut Vec<u8>, tx: &Sender<InboundEvent>, verified: Option<&str>) -> bool {
    while !buf.is_empty() {
        match Message::decode(buf) {
            Ok((msg, used)) => {
                let from = message_sender(&msg);
                // on a secure link the key names the sender; a message
                // claiming anyone else is a forgery
                if verified.is_some_and(|id| id != from) {
                    let _ = tx.send(InboundEvent::InboundConnFailed);
                    return false;
                }
                let _ = tx.send(InboundEvent::Message(from, msg));
                buf.drain(..used);
            }
            Err(DecodeError::Truncated) => break,
            Err(_) => {
                let _ = tx.send(InboundEvent::InboundConnFailed);
                return false;
            }
        }
    }
    true
}

fn message_sender(msg: &Message) -> String {
    // Every message variant carries the sender's id in a known
    // field — use that as the per-elector "from" key for the
    // orchestrator's on_message route.
    match msg {
        Message::Hb { node_id, .. } => node_id.clone(),
        Message::Offer { candidate_id, .. } => candidate_id.clone(),
        Message::Accept { accepter_id, .. } => accepter_id.clone(),
        Message::Announce { new_primary_id, .. } => new_primary_id.clone(),
    }
}

// needless_pass_by_value: thread entry point (see `accept_loop`).
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn outbound_loop(peer: PeerAddr, shared: Arc<Shared>, stop: Arc<AtomicBool>) {
    let mut stream: Option<Link> = None;
    while !stop.load(Ordering::Relaxed) {
        if stream.is_none() {
            stream = dial(&peer).and_then(|s| match &shared.secure {
                None => Some(Link::Plain(s)),
                Some(secure) => initiate(s, secure, &peer.node_id).ok(),
            });
            if stream.is_none() {
                std::thread::sleep(READ_RETRY_BACKOFF);
                continue;
            }
        }
        // Drain this peer's outbound queue.
        let next_msg = {
            let mut qs = shared.out_queues.lock().expect("out_queues lock");
            qs.get_mut(&peer.node_id).and_then(std::collections::VecDeque::pop_front)
        };
        let Some(msg) = next_msg else {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        };
        let bytes = msg.encode();
        let Some(s) = stream.as_mut() else {
            continue;
        };
        if s.write_all(&bytes).is_err() {
            // Connection died. Drop + reconnect next iter; re-
            // queue the in-flight message at the head.
            let _ = s.stream().shutdown(Shutdown::Both);
            stream = None;
            let mut qs = shared.out_queues.lock().expect("out_queues lock");
            qs.entry(peer.node_id.clone()).or_default().push_front(msg);
        }
    }
}

fn dial(peer: &PeerAddr) -> Option<TcpStream> {
    let target = (peer.host.as_str(), peer.port);
    let addr_iter = target.to_socket_addrs().ok()?;
    for sa in addr_iter {
        if let Ok(s) = TcpStream::connect_timeout(&sa, Duration::from_millis(500)) {
            let _ = s.set_nodelay(true);
            return Some(s);
        }
    }
    None
}

// needless_pass_by_value: thread entry point (see `accept_loop`).
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn orchestrator_loop(
    shared: Arc<Shared>,
    inbound_rx: Receiver<InboundEvent>,
    hb_interval: Duration,
    stop: Arc<AtomicBool>,
    on_change: TopologyCallback,
) {
    let mut last_view: Option<(crate::message::Role, Option<String>, bool)> = None;
    // Tick at hb_interval — wait up to that long on the inbound
    // channel; either a message arrives + we process it, or the
    // timeout fires + we run tick.
    while !stop.load(Ordering::Relaxed) {
        let Some(outs) = pump_inbound(&shared, &inbound_rx, hb_interval) else {
            return;
        };
        // Detect (role, primary) transitions and notify.
        {
            let now = Instant::now();
            let e = shared.elector.lock().expect("elector lock");
            let view = (e.role(), e.current_primary().map(str::to_string), e.has_quorum(now));
            drop(e);
            let view_key = (view.0, view.1.clone(), view.2);
            if last_view.as_ref() != Some(&view_key) {
                on_change(view.0, view.1, view.2);
                last_view = Some(view_key);
            }
        }
        if !outs.is_empty() {
            enqueue_outs(&shared, outs);
        }
    }
}

/// One orchestrator pump: wait up to `hb_interval` for an inbound
/// event, drive `on_message` / `tick` against the elector, and
/// return the outbound batch. `None` means the inbound channel
/// disconnected — the orchestrator must exit.
fn pump_inbound(
    shared: &Arc<Shared>,
    inbound_rx: &Receiver<InboundEvent>,
    hb_interval: Duration,
) -> Option<Vec<Outbound>> {
    let mut outs: Vec<Outbound> = Vec::new();
    match inbound_rx.recv_timeout(hb_interval) {
        Ok(InboundEvent::Message(from, msg)) => {
            let now = Instant::now();
            let mut e = shared.elector.lock().expect("elector lock");
            outs.extend(e.on_message(&from, msg, now));
            outs.extend(e.tick(now));
        }
        Ok(InboundEvent::InboundConnFailed) => {
            // Logged elsewhere; no elector state change here
            // (DOWN detection is driven by the lack of HBs, not
            // by the absence of a TCP socket).
        }
        Err(RecvTimeoutError::Timeout) => {
            let now = Instant::now();
            let mut e = shared.elector.lock().expect("elector lock");
            outs.extend(e.tick(now));
        }
        Err(RecvTimeoutError::Disconnected) => return None,
    }
    Some(outs)
}

/// Fan an outbound batch into the per-peer queues (expanding the
/// broadcast sentinel), respecting the per-peer pending cap.
fn enqueue_outs(shared: &Arc<Shared>, outs: Vec<Outbound>) {
    let mut qs = shared.out_queues.lock().expect("out_queues lock");
    for out in outs {
        let targets: Vec<String> = if out.to == Outbound::BROADCAST {
            // Broadcast: enqueue to every peer that has a
            // queue (which is all of them — pre-seeded at
            // first outbound to that peer).
            qs.keys().cloned().collect()
        } else {
            vec![out.to]
        };
        for target in targets {
            let q = qs.entry(target).or_default();
            if q.len() < MAX_PENDING_PER_PEER {
                q.push_back(out.msg.clone());
            }
        }
    }
}

#[cfg(test)]
mod sender_key_tests {
    use super::message_sender;
    use crate::message::{Message, Role};

    /// Every variant answers with the id of the node that sent it.
    ///
    /// The orchestrator routes `on_message` by this key, so a variant that
    /// returned the wrong field would deliver a peer's message under
    /// another peer's name — and every arm reads a *differently named*
    /// field, which is precisely the shape a copy-paste gets wrong.
    ///
    /// It is a pure four-arm match, but three of its arms were reaching the
    /// dead set on some runs and not others: the election tests exercise it
    /// only through whichever messages a real election happened to exchange
    /// inside the test window. Which variants those are is a matter of
    /// timing; which variants exist is not.
    #[test]
    fn every_variant_reports_its_own_sender() {
        let hb =
            Message::Hb { epoch: 7, node_id: "n-hb".into(), role: Role::Primary, repl_offset: 1 };
        let offer = Message::Offer { new_epoch: 8, candidate_id: "n-offer".into(), repl_offset: 2 };
        let accept = Message::Accept { epoch: 8, accepter_id: "n-accept".into() };
        let announce = Message::Announce {
            epoch: 8,
            new_primary_id: "n-announce".into(),
            new_primary_addr: "127.0.0.1:6379".into(),
        };

        assert_eq!(message_sender(&hb), "n-hb");
        assert_eq!(message_sender(&offer), "n-offer");
        assert_eq!(message_sender(&accept), "n-accept");
        assert_eq!(message_sender(&announce), "n-announce");
    }
}

#[cfg(test)]
mod tests {
    use super::InboundEvent;
    use super::drain_frames;
    use crate::message::Message;

    fn hb(from: &str) -> Vec<u8> {
        (Message::Hb {
            node_id: from.to_string(),
            epoch: 1,
            role: crate::message::Role::Replica,
            repl_offset: 0,
        })
        .encode()
    }

    #[test]
    fn a_verified_link_drops_a_message_that_claims_another_sender() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut buf = hb("b");
        assert!(drain_frames(&mut buf, &tx, Some("b")));
        assert!(matches!(rx.try_recv(), Ok(InboundEvent::Message(from, _)) if from == "b"));
        let mut forged = hb("c");
        assert!(!drain_frames(&mut forged, &tx, Some("b")));
        assert!(matches!(rx.try_recv(), Ok(InboundEvent::InboundConnFailed)));
        // an unverified (plain) link keeps today's behaviour
        let mut plain = hb("c");
        assert!(drain_frames(&mut plain, &tx, None));
    }
}

/// Spawn the accept-side listener thread, appending its handle.
pub(crate) fn spawn_listener_thread(
    listener: TcpListener,
    tx: Sender<InboundEvent>,
    stop: Arc<AtomicBool>,
    shared: &Arc<Shared>,
    handles: &mut Vec<JoinHandle<()>>,
) -> std::io::Result<()> {
    let shared = Arc::clone(shared);
    handles.push(std::thread::Builder::new().name("kevy-elect-listener".to_string()).spawn(
        move || {
            accept_loop(listener, tx, stop, shared);
        },
    )?);
    Ok(())
}

/// Spawn one outbound worker thread per peer, appending the handles.
pub(crate) fn spawn_outbound_threads(
    peers: &[PeerAddr],
    shared: &Arc<Shared>,
    stop: &Arc<AtomicBool>,
    handles: &mut Vec<JoinHandle<()>>,
) -> std::io::Result<()> {
    for peer in peers {
        let peer_stop = stop.clone();
        let peer_shared = shared.clone();
        let peer_clone = peer.clone();
        handles.push(
            std::thread::Builder::new().name(format!("kevy-elect-out-{}", peer.node_id)).spawn(
                move || {
                    outbound_loop(peer_clone, peer_shared, peer_stop);
                },
            )?,
        );
    }
    Ok(())
}

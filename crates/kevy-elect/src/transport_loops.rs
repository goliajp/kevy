//! Per-thread loop bodies for [`crate::transport::Transport`] —
//! pulled out of `transport.rs` so that file stays under the
//! project's 500-LOC ceiling. The listener / per-peer outbound /
//! orchestrator threads spawned by `Transport::spawn_with_callback`
//! run these functions; the handle type, shared state, and spawn
//! plumbing stay in `transport.rs`.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crate::elector::Outbound;
use crate::message::Message;
use crate::transport::{
    InboundEvent, MAX_PENDING_PER_PEER, PeerAddr, READ_BUF_CAP, READ_RETRY_BACKOFF, Shared,
    TopologyCallback,
};
use crate::wire::{DecodeError, decode, encode};

// needless_pass_by_value: thread entry point — it owns its channel/flag for
// the thread's whole lifetime; references cannot cross `thread::spawn`.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn accept_loop(listener: TcpListener, tx: Sender<InboundEvent>, stop: Arc<AtomicBool>) {
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
                let addr_str = addr.to_string();
                let _ = std::thread::Builder::new()
                    .name(format!("kevy-elect-in-{addr_str}"))
                    .spawn(move || {
                        inbound_read_loop(stream, addr_str, tx_clone, stop_clone);
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
    mut stream: TcpStream,
    peer_addr: String,
    tx: Sender<InboundEvent>,
    stop: Arc<AtomicBool>,
) {
    let _ = stream.set_nodelay(true);
    // Short read timeout so the loop can observe `stop` between
    // reads. Blocking read otherwise can't be interrupted by a
    // flag.
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let mut buf: Vec<u8> = Vec::with_capacity(READ_BUF_CAP);
    let mut chunk = [0u8; 1024];
    while !stop.load(Ordering::Relaxed) {
        match stream.read(&mut chunk) {
            Ok(0) => {
                let _ = tx.send(InboundEvent::InboundConnFailed(peer_addr.clone()));
                return;
            }
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > READ_BUF_CAP {
                    let _ = tx.send(InboundEvent::InboundConnFailed(peer_addr.clone()));
                    return;
                }
                if !drain_frames(&mut buf, &tx, &peer_addr) {
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
                let _ = tx.send(InboundEvent::InboundConnFailed(peer_addr.clone()));
                return;
            }
        }
    }
}

/// Decode + dispatch every complete frame sitting in `buf`. Returns
/// `false` when the framing is busted — an `InboundConnFailed` has
/// been sent and the caller must drop the connection.
fn drain_frames(buf: &mut Vec<u8>, tx: &Sender<InboundEvent>, peer_addr: &str) -> bool {
    while !buf.is_empty() {
        match decode(buf) {
            Ok((msg, used)) => {
                let from = message_sender(&msg);
                let _ = tx.send(InboundEvent::Message(from, msg));
                buf.drain(..used);
            }
            Err(DecodeError::Truncated) => break,
            Err(_) => {
                let _ = tx.send(InboundEvent::InboundConnFailed(peer_addr.to_string()));
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
    let mut stream: Option<TcpStream> = None;
    while !stop.load(Ordering::Relaxed) {
        if stream.is_none() {
            stream = dial(&peer);
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
        let bytes = encode(&msg);
        let Some(s) = stream.as_mut() else {
            continue;
        };
        if s.write_all(&bytes).is_err() {
            // Connection died. Drop + reconnect next iter; re-
            // queue the in-flight message at the head.
            let _ = s.shutdown(Shutdown::Both);
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
        Ok(InboundEvent::InboundConnFailed(_)) => {
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

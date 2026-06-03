//! In-process pub/sub bus for embedded `Store`.
//!
//! Mirrors the Redis/kevy server pub/sub semantics inside a single process:
//! `Store::publish` walks the channel + pattern subscriber tables and
//! enqueues a [`PubsubFrame`] onto each matching [`Subscription`]'s
//! `std::sync::mpsc` channel. Each `Subscription` drains its own queue via
//! [`Subscription::recv`] / [`Subscription::recv_timeout`] /
//! [`Subscription::try_recv`].
//!
//! The bus lives inside `Inner` and is reached only under the embedded
//! mutex; per-publish we clone the matching senders out, drop the lock,
//! then `send()` — so a slow receiver can't stall publishes on unrelated
//! channels.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kevy_store::glob_match;

use crate::store::Inner;

/// One pub/sub event delivered to a [`Subscription`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PubsubFrame {
    /// Ack: `SUBSCRIBE` succeeded on `channel`.
    Subscribe {
        /// Channel that was just subscribed.
        channel: Vec<u8>,
        /// Total channels + patterns this subscription holds after the op.
        count: usize,
    },
    /// Ack: `PSUBSCRIBE` succeeded on `pattern`.
    Psubscribe {
        /// Pattern that was just subscribed.
        pattern: Vec<u8>,
        /// Total channels + patterns this subscription holds after the op.
        count: usize,
    },
    /// Ack: `UNSUBSCRIBE` removed `channel` (or "all", when `None`).
    Unsubscribe {
        /// Channel that was just unsubscribed (`None` = "all").
        channel: Option<Vec<u8>>,
        /// Total channels + patterns still held after the op.
        count: usize,
    },
    /// Ack: `PUNSUBSCRIBE` removed `pattern` (or "all", when `None`).
    Punsubscribe {
        /// Pattern that was just unsubscribed (`None` = "all").
        pattern: Option<Vec<u8>>,
        /// Total channels + patterns still held after the op.
        count: usize,
    },
    /// A `PUBLISH` reached a channel this subscription holds directly.
    Message {
        /// Channel the publish was made to.
        channel: Vec<u8>,
        /// Raw payload bytes.
        payload: Vec<u8>,
    },
    /// A `PUBLISH` reached a channel matching one of this subscription's
    /// patterns.
    Pmessage {
        /// Pattern the channel matched.
        pattern: Vec<u8>,
        /// Channel the publish was made to.
        channel: Vec<u8>,
        /// Raw payload bytes.
        payload: Vec<u8>,
    },
}

/// Internal entry in the bus tables.
struct BusEntry {
    id: u64,
    sender: Sender<PubsubFrame>,
}

/// The pub/sub registry, owned by `Inner`.
pub(crate) struct PubsubBus {
    next_id: u64,
    channels: HashMap<Vec<u8>, Vec<BusEntry>>,
    patterns: Vec<(Vec<u8>, BusEntry)>,
}

impl PubsubBus {
    pub(crate) fn new() -> Self {
        Self {
            next_id: 1,
            channels: HashMap::new(),
            patterns: Vec::new(),
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = id.wrapping_add(1).max(1);
        id
    }

    /// Total channels + patterns the given subscription id is bound to.
    fn count_for(&self, id: u64) -> usize {
        let chans = self
            .channels
            .values()
            .filter(|v| v.iter().any(|e| e.id == id))
            .count();
        let pats = self.patterns.iter().filter(|(_, e)| e.id == id).count();
        chans + pats
    }

    /// Build the per-publish delivery plan: a list of (frame, sender)
    /// pairs. Caller drops the bus lock before invoking `send()` so a
    /// slow receiver can't stall unrelated traffic.
    pub(crate) fn collect_delivery(
        &self,
        channel: &[u8],
        payload: &[u8],
    ) -> Vec<(PubsubFrame, Sender<PubsubFrame>)> {
        let mut plans = Vec::new();
        if let Some(subs) = self.channels.get(channel) {
            for e in subs {
                plans.push((
                    PubsubFrame::Message {
                        channel: channel.to_vec(),
                        payload: payload.to_vec(),
                    },
                    e.sender.clone(),
                ));
            }
        }
        for (pat, e) in &self.patterns {
            if glob_match(pat, channel) {
                plans.push((
                    PubsubFrame::Pmessage {
                        pattern: pat.clone(),
                        channel: channel.to_vec(),
                        payload: payload.to_vec(),
                    },
                    e.sender.clone(),
                ));
            }
        }
        plans
    }

    fn add_channel(&mut self, id: u64, sender: &Sender<PubsubFrame>, channel: Vec<u8>) -> bool {
        let subs = self.channels.entry(channel).or_default();
        if subs.iter().any(|e| e.id == id) {
            return false;
        }
        subs.push(BusEntry {
            id,
            sender: sender.clone(),
        });
        true
    }

    fn add_pattern(&mut self, id: u64, sender: &Sender<PubsubFrame>, pattern: Vec<u8>) -> bool {
        if self
            .patterns
            .iter()
            .any(|(p, e)| p == &pattern && e.id == id)
        {
            return false;
        }
        self.patterns.push((
            pattern,
            BusEntry {
                id,
                sender: sender.clone(),
            },
        ));
        true
    }

    fn remove_channel(&mut self, id: u64, channel: &[u8]) -> bool {
        if let Some(subs) = self.channels.get_mut(channel) {
            let before = subs.len();
            subs.retain(|e| e.id != id);
            let removed = subs.len() < before;
            if subs.is_empty() {
                self.channels.remove(channel);
            }
            removed
        } else {
            false
        }
    }

    fn remove_pattern(&mut self, id: u64, pattern: &[u8]) -> bool {
        let before = self.patterns.len();
        self.patterns.retain(|(p, e)| !(p == pattern && e.id == id));
        self.patterns.len() < before
    }

    fn remove_all_for(&mut self, id: u64) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let mut chans = Vec::new();
        let mut pats = Vec::new();
        self.channels.retain(|name, subs| {
            let had = subs.iter().any(|e| e.id == id);
            if had {
                chans.push(name.clone());
            }
            subs.retain(|e| e.id != id);
            !subs.is_empty()
        });
        self.patterns.retain(|(p, e)| {
            if e.id == id {
                pats.push(p.clone());
                false
            } else {
                true
            }
        });
        (chans, pats)
    }
}

/// A handle to one subscription — owns the receive end of the bus channel.
///
/// Drop unsubscribes from everything automatically. While the handle is
/// alive, [`recv`](Self::recv) / [`recv_timeout`](Self::recv_timeout) /
/// [`try_recv`](Self::try_recv) drain queued [`PubsubFrame`]s in arrival
/// order.
#[allow(missing_debug_implementations)]
pub struct Subscription {
    inner: Arc<Mutex<Inner>>,
    // Keeps the AOF/reaper alive as long as a Subscription does — so
    // dropping every `Store` clone while a subscriber is still active
    // leaves the keyspace intact until the subscriber also goes away.
    _guard: Arc<crate::store::DropGuard>,
    receiver: Receiver<PubsubFrame>,
    sender: Sender<PubsubFrame>,
    id: u64,
    channels: HashSet<Vec<u8>>,
    patterns: HashSet<Vec<u8>>,
}

impl Subscription {
    pub(crate) fn new(inner: Arc<Mutex<Inner>>, guard: Arc<crate::store::DropGuard>) -> Self {
        let (sender, receiver) = channel();
        let id = inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .bus
            .alloc_id();
        Self {
            inner,
            _guard: guard,
            receiver,
            sender,
            id,
            channels: HashSet::new(),
            patterns: HashSet::new(),
        }
    }

    /// `SUBSCRIBE channel [channel ...]`. Per-channel `Subscribe` acks are
    /// enqueued onto the receive queue in order.
    pub fn subscribe(&mut self, channels: &[&[u8]]) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        for ch in channels {
            let owned = ch.to_vec();
            let added = g.bus.add_channel(self.id, &self.sender, owned.clone());
            if added {
                self.channels.insert(owned.clone());
            }
            let count = g.bus.count_for(self.id);
            let _ = self.sender.send(PubsubFrame::Subscribe {
                channel: owned,
                count,
            });
        }
    }

    /// `PSUBSCRIBE pattern [pattern ...]`. Patterns use Redis glob syntax
    /// (`*`, `?`, `[abc]`).
    pub fn psubscribe(&mut self, patterns: &[&[u8]]) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        for pat in patterns {
            let owned = pat.to_vec();
            let added = g.bus.add_pattern(self.id, &self.sender, owned.clone());
            if added {
                self.patterns.insert(owned.clone());
            }
            let count = g.bus.count_for(self.id);
            let _ = self.sender.send(PubsubFrame::Psubscribe {
                pattern: owned,
                count,
            });
        }
    }

    /// `UNSUBSCRIBE [channel ...]`. Empty `channels` removes every channel
    /// subscription this handle holds (matching the Redis wire shape:
    /// individual ack frames for each channel that was actually removed,
    /// or a single `Unsubscribe { channel: None }` if none were held).
    pub fn unsubscribe(&mut self, channels: &[&[u8]]) {
        if channels.is_empty() {
            self.drain_channel_subs();
            return;
        }
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        for ch in channels {
            let owned = ch.to_vec();
            let _ = g.bus.remove_channel(self.id, &owned);
            self.channels.remove(&owned);
            let count = g.bus.count_for(self.id);
            let _ = self.sender.send(PubsubFrame::Unsubscribe {
                channel: Some(owned),
                count,
            });
        }
    }

    /// `PUNSUBSCRIBE [pattern ...]`. Empty `patterns` removes every pattern.
    pub fn punsubscribe(&mut self, patterns: &[&[u8]]) {
        if patterns.is_empty() {
            self.drain_pattern_subs();
            return;
        }
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        for pat in patterns {
            let owned = pat.to_vec();
            let _ = g.bus.remove_pattern(self.id, &owned);
            self.patterns.remove(&owned);
            let count = g.bus.count_for(self.id);
            let _ = self.sender.send(PubsubFrame::Punsubscribe {
                pattern: Some(owned),
                count,
            });
        }
    }

    fn drain_channel_subs(&mut self) {
        let owned: Vec<Vec<u8>> = self.channels.drain().collect();
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if owned.is_empty() {
            let count = g.bus.count_for(self.id);
            let _ = self
                .sender
                .send(PubsubFrame::Unsubscribe { channel: None, count });
            return;
        }
        for ch in owned {
            let _ = g.bus.remove_channel(self.id, &ch);
            let count = g.bus.count_for(self.id);
            let _ = self.sender.send(PubsubFrame::Unsubscribe {
                channel: Some(ch),
                count,
            });
        }
    }

    fn drain_pattern_subs(&mut self) {
        let owned: Vec<Vec<u8>> = self.patterns.drain().collect();
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if owned.is_empty() {
            let count = g.bus.count_for(self.id);
            let _ = self
                .sender
                .send(PubsubFrame::Punsubscribe { pattern: None, count });
            return;
        }
        for p in owned {
            let _ = g.bus.remove_pattern(self.id, &p);
            let count = g.bus.count_for(self.id);
            let _ = self.sender.send(PubsubFrame::Punsubscribe {
                pattern: Some(p),
                count,
            });
        }
    }

    /// Block until one frame is queued. `Err(io::ErrorKind::UnexpectedEof)`
    /// once the underlying bus tears down (last `Store` clone dropped).
    pub fn recv(&self) -> io::Result<PubsubFrame> {
        self.receiver
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::UnexpectedEof, "bus closed"))
    }

    /// Bounded blocking recv. `Err(io::ErrorKind::TimedOut)` when `dur`
    /// elapses; `Err(io::ErrorKind::UnexpectedEof)` when the bus is gone.
    pub fn recv_timeout(&self, dur: Duration) -> io::Result<PubsubFrame> {
        self.receiver.recv_timeout(dur).map_err(|e| match e {
            RecvTimeoutError::Timeout => io::Error::from(io::ErrorKind::TimedOut),
            RecvTimeoutError::Disconnected => {
                io::Error::new(io::ErrorKind::UnexpectedEof, "bus closed")
            }
        })
    }

    /// Non-blocking recv. `Ok(None)` if the queue is empty;
    /// `Err(UnexpectedEof)` when the bus is gone.
    pub fn try_recv(&self) -> io::Result<Option<PubsubFrame>> {
        match self.receiver.try_recv() {
            Ok(f) => Ok(Some(f)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "bus closed"))
            }
        }
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("id", &self.id)
            .field("channels", &self.channels.len())
            .field("patterns", &self.patterns.len())
            .finish_non_exhaustive()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // Best-effort cleanup. If the underlying Inner is poisoned we still
        // remove our entries; the AtomicBool / send stuff doesn't care.
        if let Ok(mut g) = self.inner.lock() {
            g.bus.remove_all_for(self.id);
        } else if let Ok(mut g) = self.inner.clear_poison_and_lock() {
            // Mutex::clear_poison + reacquire is stable since Rust 1.77; we
            // pin rust-version=1.95 so this is available. The `else` branch
            // above is unreachable in practice given we always recover from
            // poison ourselves; left here so the cleanup is total.
            g.bus.remove_all_for(self.id);
        }
    }
}

/// Tiny helper trait so `Drop` can recover from poison without
/// pulling in the explicit `poison.into_inner()` dance. Local to the
/// module; not part of the public API.
trait LockExt<'a, T> {
    fn clear_poison_and_lock(&'a self) -> std::sync::LockResult<std::sync::MutexGuard<'a, T>>;
}

impl<'a, T> LockExt<'a, T> for Mutex<T> {
    fn clear_poison_and_lock(&'a self) -> std::sync::LockResult<std::sync::MutexGuard<'a, T>> {
        self.clear_poison();
        self.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Store};

    fn store() -> Store {
        Store::open(Config::default().with_ttl_reaper_manual()).unwrap()
    }

    #[test]
    fn publish_to_no_subscribers_returns_zero() {
        let s = store();
        assert_eq!(s.publish(b"chan", b"hi"), 0);
    }

    #[test]
    fn subscribe_ack_then_message_delivered() {
        let s = store();
        let sub = s.subscribe(&[b"news"]);
        // Drain the SUBSCRIBE ack.
        assert_eq!(
            sub.recv().unwrap(),
            PubsubFrame::Subscribe {
                channel: b"news".to_vec(),
                count: 1,
            }
        );
        // Same store handle (or a clone) can publish.
        assert_eq!(s.publish(b"news", b"hello"), 1);
        assert_eq!(
            sub.recv().unwrap(),
            PubsubFrame::Message {
                channel: b"news".to_vec(),
                payload: b"hello".to_vec(),
            }
        );
    }

    #[test]
    fn store_clone_publishes_reach_other_clones_subscribers() {
        let s1 = store();
        let s2 = s1.clone();
        let sub = s1.subscribe(&[b"x"]);
        let _ = sub.recv().unwrap(); // ack
        assert_eq!(s2.publish(b"x", b"v"), 1);
        assert_eq!(
            sub.recv().unwrap(),
            PubsubFrame::Message {
                channel: b"x".to_vec(),
                payload: b"v".to_vec(),
            }
        );
    }

    #[test]
    fn psubscribe_glob_match_delivers_pmessage() {
        let s = store();
        let sub = s.psubscribe(&[b"news.*"]);
        let _ = sub.recv().unwrap(); // psubscribe ack
        assert_eq!(s.publish(b"news.tech", b"breaking"), 1);
        assert_eq!(
            sub.recv().unwrap(),
            PubsubFrame::Pmessage {
                pattern: b"news.*".to_vec(),
                channel: b"news.tech".to_vec(),
                payload: b"breaking".to_vec(),
            }
        );
        // Non-matching publish does not reach the subscriber.
        assert_eq!(s.publish(b"weather", b"sunny"), 0);
        assert!(sub.try_recv().unwrap().is_none());
    }

    #[test]
    fn duplicate_subscribe_does_not_duplicate_delivery() {
        let s = store();
        let mut sub = s.subscribe(&[b"x"]);
        sub.subscribe(&[b"x"]); // second call to same channel: no-op
        // Drain the two acks (one from subscribe(), one from the second call).
        let a1 = sub.recv().unwrap();
        let a2 = sub.recv().unwrap();
        assert!(matches!(a1, PubsubFrame::Subscribe { count: 1, .. }));
        assert!(matches!(a2, PubsubFrame::Subscribe { count: 1, .. }));
        // Single delivery, despite "double subscribe".
        assert_eq!(s.publish(b"x", b"v"), 1);
        let _ = sub.recv().unwrap();
        assert!(sub.try_recv().unwrap().is_none());
    }

    #[test]
    fn unsubscribe_removes_then_no_more_messages() {
        let s = store();
        let mut sub = s.subscribe(&[b"x"]);
        let _ = sub.recv().unwrap();
        sub.unsubscribe(&[b"x"]);
        // Drain the unsubscribe ack.
        assert!(matches!(
            sub.recv().unwrap(),
            PubsubFrame::Unsubscribe {
                channel: Some(_),
                count: 0
            }
        ));
        // Publishes no longer reach us.
        assert_eq!(s.publish(b"x", b"v"), 0);
    }

    #[test]
    fn unsubscribe_all_with_empty_args_drains_every_channel() {
        let s = store();
        let mut sub = s.subscribe(&[b"a", b"b"]);
        let _ = sub.recv().unwrap();
        let _ = sub.recv().unwrap();
        sub.unsubscribe(&[]);
        // Two unsubscribe acks, one per removed channel.
        for _ in 0..2 {
            assert!(matches!(
                sub.recv().unwrap(),
                PubsubFrame::Unsubscribe {
                    channel: Some(_),
                    ..
                }
            ));
        }
        // Publishes go nowhere now.
        assert_eq!(s.publish(b"a", b"x"), 0);
        assert_eq!(s.publish(b"b", b"x"), 0);
    }

    #[test]
    fn unsubscribe_when_no_subs_held_emits_nil_channel_ack() {
        let s = store();
        let mut sub = s.subscribe(&[]); // empty start
        sub.unsubscribe(&[]);
        assert!(matches!(
            sub.recv().unwrap(),
            PubsubFrame::Unsubscribe {
                channel: None,
                count: 0
            }
        ));
    }

    #[test]
    fn drop_subscriber_unregisters() {
        let s = store();
        let sub = s.subscribe(&[b"x"]);
        let _ = sub.recv().unwrap();
        assert_eq!(s.publish(b"x", b"v"), 1);
        let _ = sub.recv().unwrap();
        drop(sub);
        assert_eq!(s.publish(b"x", b"v"), 0);
    }

    #[test]
    fn recv_timeout_returns_timeout_when_empty() {
        let s = store();
        let sub = s.subscribe(&[b"x"]);
        // Drain the ack first.
        let _ = sub.recv_timeout(Duration::from_millis(100)).unwrap();
        let err = sub
            .recv_timeout(Duration::from_millis(50))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }
}

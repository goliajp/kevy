//! In-process pub/sub bus for embedded `Store`.
//!
//! Mirrors the Redis/kevy server pub/sub semantics inside a single process:
//! `Store::publish` walks the channel + pattern subscriber tables and
//! enqueues a [`PubsubEvent`] onto each matching [`Subscription`]'s
//! `std::sync::mpsc` channel. Each `Subscription` drains its own queue via
//! [`Subscription::recv`] / [`Subscription::recv_timeout`] /
//! [`Subscription::try_recv`].
//!
//! The bus lives inside `Inner` and is reached only under the embedded
//! mutex; per-publish we clone the matching senders out, drop the lock,
//! then `send()` — so a slow receiver can't stall publishes on unrelated
//! channels.

// A send to a subscriber that has gone away is the normal end of a
// subscription, not an error to handle: the receiver drops when the
// client disconnects, and the bus removes it on the next sweep.
// Reporting here would turn every ordinary disconnect into a log line.
#![expect(
    clippy::let_underscore_must_use,
    reason = "a dropped receiver is how a subscription ends"
)]

use crate::{KevyError, KevyResult};
use std::collections::HashSet;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crate::store::Inner;

/// One pub/sub event delivered to a [`Subscription`] — the same type the
/// network clients receive, so a consumer handles either source alike.
pub use kevy_resp::PubsubEvent;

// `BusEntry` + `PubsubBus` live in [`crate::pubsub_bus`] — split out so
// this file stays under the 500-LOC house rule. Re-exported below so
// `crate::store::Inner` keeps its existing `pubsub::PubsubBus` import.
pub(crate) use crate::pubsub_bus::PubsubBus;

/// A handle to one subscription — owns the receive end of the bus channel.
///
/// Drop unsubscribes from everything automatically. While the handle is
/// alive, [`recv`](Self::recv) / [`recv_timeout`](Self::recv_timeout) /
/// [`try_recv`](Self::try_recv) drain queued [`PubsubEvent`]s in arrival
/// order.
///
/// **Threading.** `Subscription` is `Send + Sync` —
/// `Arc<Subscription>` works, so multiple async tasks (or
/// `spawn_blocking` jobs) can share one subscription and call `recv`
/// concurrently. The underlying `std::sync::mpsc::Receiver` is
/// !Sync, so we wrap it (and the matching ack `Sender`) in a `Mutex`;
/// concurrent `recv` callers serialise on that lock, with each call
/// receiving a *different* frame in arrival order (single-consumer
/// semantics — NOT broadcast fanout). `try_recv` is non-blocking even
/// under contention: if the lock is held by a blocking `recv`,
/// `try_recv` returns `Ok(None)` rather than waiting.
///
/// If you need broadcast fanout (every subscriber sees every message),
/// open a separate `Subscription` per consumer — they're cheap.
///
/// ```
/// use kevy_embedded::{Config, PubsubEvent, Store};
///
/// let s = Store::open(Config::default())?;
/// let sub = s.subscribe(&[b"news"]);
/// // the subscribe ack is queued first
/// assert!(matches!(sub.recv()?, PubsubEvent::Subscribe { count: 1, .. }));
/// assert_eq!(s.publish(b"news", b"hello"), 1, "one subscriber received it");
/// let PubsubEvent::Message { channel, payload } = sub.recv()? else {
///     panic!("a publish arrives as a message");
/// };
/// assert_eq!((channel, payload), (b"news".to_vec(), b"hello".to_vec()));
/// drop(sub); // unsubscribes
/// assert_eq!(s.publish(b"news", b"again"), 0);
/// # Ok::<(), kevy_embedded::KevyError>(())
/// ```
#[allow(missing_debug_implementations)]
pub struct Subscription {
    inner: Arc<RwLock<Inner>>,
    // Keeps the AOF/reaper alive as long as a Subscription does — so
    // dropping every `Store` clone while a subscriber is still active
    // leaves the keyspace intact until the subscriber also goes away.
    _guard: Arc<crate::store::DropGuard>,
    // `Receiver<T>` is `Send + !Sync`; wrap so `Subscription: Sync`.
    // Hot path (recv) acquires + holds the lock during the blocking
    // wait — single consumer at a time; concurrent recv callers
    // serialise and each get a different frame. See type-level
    // doc-comment for the trade-off.
    receiver: Mutex<Receiver<PubsubEvent>>,
    // `Sender<T>` is also !Sync (Send + Clone but cannot be shared by
    // reference across threads). Wrap so the ack-frame path (called
    // from subscribe/unsubscribe / Drop) can run from any thread.
    sender: Mutex<Sender<PubsubEvent>>,
    id: u64,
    channels: HashSet<Vec<u8>>,
    patterns: HashSet<Vec<u8>>,
}

impl Subscription {
    pub(crate) fn new(inner: Arc<RwLock<Inner>>, guard: Arc<crate::store::DropGuard>) -> Self {
        let (sender, receiver) = channel();
        let id = inner.write().unwrap_or_else(std::sync::PoisonError::into_inner).bus.alloc_id();
        Self {
            inner,
            _guard: guard,
            receiver: Mutex::new(receiver),
            sender: Mutex::new(sender),
            id,
            channels: HashSet::new(),
            patterns: HashSet::new(),
        }
    }

    /// Clone of the inbound `Sender`. Used both for ack frames (Subscribe /
    /// Unsubscribe / ...) and to register a sender clone inside
    /// `PubsubBus`. Calling this acquires the sender lock briefly (~20 ns).
    fn sender_clone(&self) -> Sender<PubsubEvent> {
        self.sender.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// `SUBSCRIBE channel [channel ...]`. Per-channel `Subscribe` acks are
    /// enqueued onto the receive queue in order.
    pub fn subscribe(&mut self, channels: &[&[u8]]) {
        let s = self.sender_clone();
        let mut g = self.inner.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        for ch in channels {
            let owned = ch.to_vec();
            let added = g.bus.add_channel(self.id, &s, owned.clone());
            if added {
                self.channels.insert(owned.clone());
            }
            let count = g.bus.count_for(self.id);
            let _ = s.send(PubsubEvent::Subscribe { channel: owned, count: count as i64 });
        }
    }

    /// `PSUBSCRIBE pattern [pattern ...]`. Patterns use Redis glob syntax
    /// (`*`, `?`, `[abc]`).
    pub fn psubscribe(&mut self, patterns: &[&[u8]]) {
        let s = self.sender_clone();
        let mut g = self.inner.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        for pat in patterns {
            let owned = pat.to_vec();
            let added = g.bus.add_pattern(self.id, &s, owned.clone());
            if added {
                self.patterns.insert(owned.clone());
            }
            let count = g.bus.count_for(self.id);
            let _ = s.send(PubsubEvent::Psubscribe { pattern: owned, count: count as i64 });
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
        let s = self.sender_clone();
        let mut g = self.inner.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        for ch in channels {
            let owned = ch.to_vec();
            let _ = g.bus.remove_channel(self.id, &owned);
            self.channels.remove(&owned);
            let count = g.bus.count_for(self.id);
            let _ = s.send(PubsubEvent::Unsubscribe { channel: Some(owned), count: count as i64 });
        }
    }

    /// `PUNSUBSCRIBE [pattern ...]`. Empty `patterns` removes every pattern.
    pub fn punsubscribe(&mut self, patterns: &[&[u8]]) {
        if patterns.is_empty() {
            self.drain_pattern_subs();
            return;
        }
        let s = self.sender_clone();
        let mut g = self.inner.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        for pat in patterns {
            let owned = pat.to_vec();
            let _ = g.bus.remove_pattern(self.id, &owned);
            self.patterns.remove(&owned);
            let count = g.bus.count_for(self.id);
            let _ = s.send(PubsubEvent::Punsubscribe { pattern: Some(owned), count: count as i64 });
        }
    }

    fn drain_channel_subs(&mut self) {
        let s = self.sender_clone();
        let owned: Vec<Vec<u8>> = self.channels.drain().collect();
        let mut g = self.inner.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if owned.is_empty() {
            let count = g.bus.count_for(self.id);
            let _ = s.send(PubsubEvent::Unsubscribe { channel: None, count: count as i64 });
            return;
        }
        for ch in owned {
            let _ = g.bus.remove_channel(self.id, &ch);
            let count = g.bus.count_for(self.id);
            let _ = s.send(PubsubEvent::Unsubscribe { channel: Some(ch), count: count as i64 });
        }
    }

    fn drain_pattern_subs(&mut self) {
        let s = self.sender_clone();
        let owned: Vec<Vec<u8>> = self.patterns.drain().collect();
        let mut g = self.inner.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if owned.is_empty() {
            let count = g.bus.count_for(self.id);
            let _ = s.send(PubsubEvent::Punsubscribe { pattern: None, count: count as i64 });
            return;
        }
        for p in owned {
            let _ = g.bus.remove_pattern(self.id, &p);
            let count = g.bus.count_for(self.id);
            let _ = s.send(PubsubEvent::Punsubscribe { pattern: Some(p), count: count as i64 });
        }
    }

    /// Block until one frame is queued. `Err(io::ErrorKind::UnexpectedEof)`
    /// once the underlying bus tears down (last `Store` clone dropped).
    ///
    /// Acquires the receiver mutex for the entire blocking wait — other
    /// `recv`/`recv_timeout` callers serialise behind this one. Concurrent
    /// `try_recv` calls return `Ok(None)` while a `recv` is blocked (no
    /// wait on the lock); see the type-level doc for the trade-off.
    pub fn recv(&self) -> KevyResult<PubsubEvent> {
        let g = self.receiver.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.recv().map_err(|_| KevyError::Closed)
    }

    /// Bounded blocking recv. `Err(KevyError::TimedOut)` when `dur`
    /// elapses; `Err(KevyError::Closed)` when the bus is gone.
    pub fn recv_timeout(&self, dur: Duration) -> KevyResult<PubsubEvent> {
        let g = self.receiver.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.recv_timeout(dur).map_err(|e| match e {
            RecvTimeoutError::Timeout => KevyError::TimedOut,
            RecvTimeoutError::Disconnected => KevyError::Closed,
        })
    }

    /// Non-blocking recv. `Ok(None)` if the queue is empty;
    /// `Err(KevyError::Closed)` when the bus is gone.
    ///
    /// Uses `try_lock` so a concurrent blocking `recv` doesn't make
    /// `try_recv` itself block — lock contention is reported as `Ok(None)`
    /// (semantically: "no frame available right now"). Same shape callers
    /// already handle for an empty queue.
    pub fn try_recv(&self) -> KevyResult<Option<PubsubEvent>> {
        let Ok(g) = self.receiver.try_lock() else {
            return Ok(None);
        };
        match g.try_recv() {
            Ok(f) => Ok(Some(f)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(KevyError::Closed),
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
        // Best-effort cleanup. Recover from poison (a panic elsewhere left the
        // bus intact) so our entries are always removed.
        let mut g = self.inner.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.bus.remove_all_for(self.id);
    }
}

#[cfg(test)]
#[path = "pubsub_tests.rs"]
mod tests;

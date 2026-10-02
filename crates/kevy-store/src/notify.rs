//! Store-origin keyspace-event capture: `new` (a key was created),
//! `expired` (a TTL'd key was dropped — lazily on access or by the
//! active reaper), and `evicted` (maxmemory pressure removed a key).
//!
//! These events originate INSIDE store operations, where no pub/sub
//! machinery is in reach, so the store records them into a buffer the
//! serving layer drains and publishes (after each write, and on the
//! shard tick for reaper-origin batches). Capture is opt-in per kind
//! — with the mask at its all-off default every hook is a single
//! predicted-not-taken byte test, so embedders and disabled servers
//! pay nothing.

use crate::Store;
#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;

/// One captured store-origin event kind; [`KeyspaceEvent::name`] is the
/// Redis event name the serving layer publishes it under.
///
/// ```
/// assert_eq!(kevy_store::KeyspaceEvent::Evicted.name(), "evicted");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KeyspaceEvent {
    /// A key was added to the keyspace.
    ///
    /// ```
    /// use kevy_store::{KeyspaceEvent, SetCondition, Store};
    /// let mut s = Store::new();
    /// s.set_notify_capture([KeyspaceEvent::New]);
    /// s.set(b"k", b"v".to_vec(), None, SetCondition::Always);
    /// s.set(b"k", b"w".to_vec(), None, SetCondition::Always); // not new
    /// assert_eq!(s.take_notify_events(), [(KeyspaceEvent::New, b"k".to_vec())]);
    /// ```
    New,
    /// A TTL'd key was removed because its deadline passed.
    ///
    /// ```
    /// use core::time::Duration;
    /// use kevy_store::{KeyspaceEvent, SetCondition, Store};
    /// let mut s = Store::new();
    /// s.set_notify_capture([KeyspaceEvent::Expired]);
    /// s.set(b"k", b"v".to_vec(), Some(Duration::from_millis(1)), SetCondition::Always);
    /// while s.get(b"k")?.is_some() {
    ///     std::thread::sleep(Duration::from_millis(1));
    /// }
    /// assert_eq!(s.take_notify_events(), [(KeyspaceEvent::Expired, b"k".to_vec())]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Expired,
    /// A key was removed by maxmemory eviction.
    ///
    /// ```
    /// use kevy_store::{EvictionPolicy, KeyspaceEvent, SetCondition, Store};
    /// let mut s = Store::new();
    /// s.set_notify_capture([KeyspaceEvent::Evicted]);
    /// s.set_max_memory(1, EvictionPolicy::AllKeysLru);
    /// s.set(b"k", b"v".to_vec(), None, SetCondition::Always);
    /// s.try_evict_after_write();
    /// assert_eq!(s.take_notify_events(), [(KeyspaceEvent::Evicted, b"k".to_vec())]);
    /// ```
    Evicted,
    /// A write removed a collection's last member, and the key went with
    /// it. Published as `del`, after the event of the write that did it.
    ///
    /// ```
    /// use kevy_store::{KeyspaceEvent, Store};
    /// let mut s = Store::new();
    /// s.set_notify_capture([KeyspaceEvent::Emptied]);
    /// s.sadd(b"k", &[b"only".as_slice()])?;
    /// s.srem(b"k", &[b"only".as_slice()])?;
    /// assert_eq!(s.take_notify_events(), [(KeyspaceEvent::Emptied, b"k".to_vec())]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Emptied,
}

impl KeyspaceEvent {
    /// The Redis keyspace-event name (`new` / `expired` / `evicted`).
    ///
    /// ```
    /// assert_eq!(kevy_store::KeyspaceEvent::New.name(), "new");
    /// ```
    pub fn name(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Expired => "expired",
            Self::Emptied => "del",
            Self::Evicted => "evicted",
        }
    }

    fn capture_bit(self) -> u8 {
        match self {
            Self::New => CAPTURE_NEW,
            Self::Expired => CAPTURE_EXPIRED,
            Self::Evicted => CAPTURE_EVICTED,
            Self::Emptied => CAPTURE_EMPTIED,
        }
    }
}

pub(crate) const CAPTURE_NEW: u8 = 1 << 0;
pub(crate) const CAPTURE_EXPIRED: u8 = 1 << 1;
pub(crate) const CAPTURE_EVICTED: u8 = 1 << 2;
pub(crate) const CAPTURE_EMPTIED: u8 = 1 << 3;

impl Store {
    /// Choose which store-origin event kinds to capture, replacing the
    /// previous choice. The serving layer mirrors its
    /// notify-keyspace-events flags here; none (the default) reduces
    /// every capture hook to one byte test.
    ///
    /// ```
    /// use kevy_store::{KeyspaceEvent, SetCondition, Store};
    /// let mut s = Store::new();
    /// s.set_notify_capture([KeyspaceEvent::New]);
    /// s.set(b"k", b"v".to_vec(), None, SetCondition::Always);
    /// assert_eq!(s.take_notify_events(), [(KeyspaceEvent::New, b"k".to_vec())]);
    /// s.set_notify_capture([]);
    /// s.set(b"k2", b"v".to_vec(), None, SetCondition::Always);
    /// assert!(!s.has_notify_events());
    /// ```
    pub fn set_notify_capture(&mut self, kinds: impl IntoIterator<Item = KeyspaceEvent>) {
        self.notify_capture = kinds.into_iter().fold(0, |m, k| m | k.capture_bit());
    }

    /// Whether any events are waiting to be drained (one length read).
    #[inline]
    pub fn has_notify_events(&self) -> bool {
        !self.notify_events.is_empty()
    }

    /// Whether any key has expired since the last drain.
    #[inline]
    pub fn has_expired_keys(&self) -> bool {
        !self.expired_keys.is_empty()
    }

    /// Take the keys dropped by expiry since the last drain.
    pub fn take_expired_keys(&mut self) -> Vec<Vec<u8>> {
        core::mem::take(&mut self.expired_keys)
    }

    /// Take every captured event, in capture order.
    pub fn take_notify_events(&mut self) -> Vec<(KeyspaceEvent, Vec<u8>)> {
        core::mem::take(&mut self.notify_events)
    }

    #[inline]
    pub(crate) fn note_expired(&mut self, key: &[u8]) {
        // Always, whatever the notification flags say: the serving layer
        // has to maintain derived state for this removal. Every expiry
        // path — lazy `reap`, the single-lookup read, the active
        // sampler — funnels through here, which is why the capture
        // belongs here and not at each of them.
        self.expired_keys.push(key.to_vec());
        if self.notify_capture & CAPTURE_EXPIRED != 0 {
            self.notify_events.push((KeyspaceEvent::Expired, key.to_vec()));
        }
    }

    /// Remove `key`, whose collection a write just emptied.
    pub(crate) fn remove_emptied(&mut self, key: &[u8]) {
        self.remove_entry(key);
        if self.notify_capture & CAPTURE_EMPTIED != 0 {
            self.notify_events.push((KeyspaceEvent::Emptied, key.to_vec()));
        }
    }

    #[inline]
    pub(crate) fn note_evicted(&mut self, key: &[u8]) {
        if self.notify_capture & CAPTURE_EVICTED != 0 {
            self.notify_events.push((KeyspaceEvent::Evicted, key.to_vec()));
        }
    }
}

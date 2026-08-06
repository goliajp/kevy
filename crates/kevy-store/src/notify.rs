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

/// One captured store-origin event kind. The serving layer maps these
/// to the Redis event names (`new` / `expired` / `evicted`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyspaceEvent {
    /// A key was added to the keyspace.
    New,
    /// A TTL'd key was removed because its deadline passed.
    Expired,
    /// A key was removed by maxmemory eviction.
    Evicted,
}

pub(crate) const CAPTURE_NEW: u8 = 1 << 0;
pub(crate) const CAPTURE_EXPIRED: u8 = 1 << 1;
pub(crate) const CAPTURE_EVICTED: u8 = 1 << 2;

impl Store {
    /// Choose which store-origin event kinds to capture. The serving
    /// layer mirrors its notify-keyspace-events flags here; all-off
    /// (the default) reduces every capture hook to one byte test.
    pub fn set_notify_capture(&mut self, new_key: bool, expired: bool, evicted: bool) {
        self.notify_capture = (u8::from(new_key) * CAPTURE_NEW)
            | (u8::from(expired) * CAPTURE_EXPIRED)
            | (u8::from(evicted) * CAPTURE_EVICTED);
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

    #[inline]
    pub(crate) fn note_evicted(&mut self, key: &[u8]) {
        if self.notify_capture & CAPTURE_EVICTED != 0 {
            self.notify_events.push((KeyspaceEvent::Evicted, key.to_vec()));
        }
    }
}

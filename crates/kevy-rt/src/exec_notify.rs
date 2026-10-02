//! Keyspace notification publish — the "after a write succeeds, maybe
//! fire `__keyspace@<db>__:<key>` and `__keyevent@<db>__:<event>`
//! channels" hook. Triggered from [`crate::exec_op::Shard::exec_op`]'s
//! write arms (Dispatch / Del / MSet / Flush) after the store mutation
//! has already happened.
//!
//! Default-OFF: `!notify_flags.is_active()` short-circuits before any
//! string formatting or registry read. Once enabled, the cost is one
//! cmd→class lookup + one or two cross-shard fan-outs per affected key.
//!
//! No reply slot — these publishes are server-initiated (not a
//! response to a client `PUBLISH`), so [`Shard::broadcast_notification`]
//! is a slimmed-down [`Shard::do_publish`] without the per-publisher
//! `Part::Reply(count)`.

use crate::Commands;
use crate::message::PubMsg;
use crate::shard::Shard;
use kevy_config::NotificationFlags;
use kevy_resp::ArgvView;
use kevy_resp::ops_table::NotifyKind;

impl<C: Commands> Shard<C> {
    /// Publish `payload` on `channel`, fire-and-forget. Mirrors
    /// `do_publish`'s fan-out path but does not write back a receiver
    /// count to any client.
    pub(crate) fn broadcast_notification(&mut self, channel: &[u8], payload: &[u8]) {
        let (_count, channel_bits) =
            self.pubsub.read().expect("pubsub registry").get(channel).copied().unwrap_or((0, 0));
        let (_pcount, pat_bits) = self.pattern_match_for_channel(channel);
        let bits = channel_bits | pat_bits;
        if bits == 0 {
            return;
        }
        let m: PubMsg = std::sync::Arc::new((channel.to_vec(), payload.to_vec()));
        for s in 0..self.nshards {
            if bits & (1u64 << s) == 0 {
                continue;
            }
            if s == self.id {
                self.deliver_publish(&m.0, &m.1);
            } else {
                // the flush walks only the shards this mask names
                self.publish_batch[s].push(m.clone());
                self.publish_batch_nonempty |= 1u64 << s;
            }
        }
    }

    /// Fire `__keyspace@0__:<key>` (`K` flag) and / or
    /// `__keyevent@0__:<event>` (`E` flag) for one `(key, event)` pair.
    /// Called by the per-Op notify helpers after they've already
    /// gated on `notify_flags.is_active()` + the per-class flag.
    pub(crate) fn notify_keyspace_event(&mut self, event: &[u8], key: &[u8]) {
        // Allocations are necessary — the channel string mixes a
        // fixed prefix and the key bytes. We hold both as owned Vecs
        // briefly; the broadcast helper takes them by slice and Arc's
        // up the per-target copies.
        if self.notify_flags.contains(crate::NotificationFlags::KEYSPACE) {
            let mut chan = Vec::with_capacity(b"__keyspace@0__:".len() + key.len());
            chan.extend_from_slice(b"__keyspace@0__:");
            chan.extend_from_slice(key);
            self.broadcast_notification(&chan, event);
        }
        if self.notify_flags.contains(crate::NotificationFlags::KEYEVENT) {
            let mut chan = Vec::with_capacity(b"__keyevent@0__:".len() + event.len());
            chan.extend_from_slice(b"__keyevent@0__:");
            chan.extend_from_slice(event);
            self.broadcast_notification(&chan, key);
        }
    }

    /// Single-key dispatched cmd (`Shard::run_dispatch`). Classify the verb, gate
    /// on the per-class flag, then fire one keyspace event for the cmd's
    /// key (`args[1]` per Redis convention — keyless cmds short-circuit
    /// inside `Commands::notify_class` returning `None`).
    pub(crate) fn maybe_notify_dispatch<A: ArgvView + ?Sized>(
        &mut self,
        args: &A,
        asked: Option<crate::propagation::Notify>,
    ) {
        if !self.notify_flags.is_active() {
            return;
        }
        // Store-origin events first — a `new` fired by this command
        // precedes the command's own event on the wire — except the `del`
        // of a collection the command emptied, which follows it.
        let mut emptied = Vec::new();
        if self.store.has_notify_events() {
            for (kind, key) in self.store.take_notify_events() {
                if kind == kevy_store::KeyspaceEvent::Emptied {
                    emptied.push(key);
                } else {
                    self.notify_keyspace_event(kind.name().as_bytes(), &key);
                }
            }
        }
        self.drain_expired_keys();
        match asked {
            None => self.notify_by_verb(args),
            Some(crate::propagation::Notify::Events(events)) => {
                for (class, event, key) in events {
                    if class_enabled(class, &self.notify_flags) {
                        self.notify_keyspace_event(event.as_bytes(), &key);
                    }
                }
            }
            Some(_) => {}
        }
        for key in emptied {
            self.notify_keyspace_event(b"del", &key);
        }
    }

    /// A write's default event: its verb, lower-cased, on argument 1.
    fn notify_by_verb<A: ArgvView + ?Sized>(&mut self, args: &A) {
        let Some(class) = self.commands.notify_class(args) else { return };
        if !class_enabled(class, &self.notify_flags) || args.len() < 2 {
            return;
        }
        let event = ascii_lower(&args[0]);
        self.notify_keyspace_event(&event, &args[1]);
    }
}

/// Whether `flags` enables keyspace events of `class`.
#[inline]
fn class_enabled(class: NotifyKind, flags: &NotificationFlags) -> bool {
    let flag = match class {
        NotifyKind::Generic => NotificationFlags::GENERIC,
        NotifyKind::String => NotificationFlags::STRING,
        NotifyKind::List => NotificationFlags::LIST,
        NotifyKind::Set => NotificationFlags::SET,
        NotifyKind::Hash => NotificationFlags::HASH,
        NotifyKind::Zset => NotificationFlags::ZSET,
        NotifyKind::Stream => NotificationFlags::STREAM,
        // a class with no flag here has no way to be switched on, so it
        // publishes nothing
        _ => return false,
    };
    flags.contains(flag)
}

impl<C: Commands> Shard<C> {
    /// Multi-key `DEL` — fire `del` per key.
    pub(crate) fn maybe_notify_del(&mut self, keys: &[Vec<u8>]) {
        if !self.notify_flags.is_active() {
            return;
        }
        self.drain_store_notify();
        self.drain_expired_keys();
        if !self.notify_flags.contains(crate::NotificationFlags::GENERIC) {
            return;
        }
        for k in keys {
            self.notify_keyspace_event(b"del", k);
        }
    }

    /// Multi-key `MSET` — fire `set` per key (matches Redis events.c).
    pub(crate) fn maybe_notify_mset(&mut self, pairs: &[(Vec<u8>, Vec<u8>)]) {
        if !self.notify_flags.is_active() {
            return;
        }
        self.drain_store_notify();
        self.drain_expired_keys();
        if !self.notify_flags.contains(crate::NotificationFlags::STRING) {
            return;
        }
        for (k, _) in pairs {
            self.notify_keyspace_event(b"set", k);
        }
    }

    /// Publish the store-origin events captured since the last drain
    /// (`new` / `expired` / `evicted`). The capture mask mirrors the
    /// live flags, so everything in the buffer is publishable as-is.
    /// Called on write paths (so a `new` precedes its command's class
    /// event) and on the shard tick (reaper batches + read-path lazy
    /// expiry); the steady-state cost is one length check.
    /// Maintain derived state for keys the store dropped on expiry.
    ///
    /// An expiring key is a write nobody issued: the store removes it
    /// on its own, inside a `reap` that the runtime never sees, so
    /// neither the index hook nor the WATCH bump fires — and the index
    /// keeps serving the row forever. Measured before the fix: a key
    /// with a 50 ms TTL was gone from `EXISTS` and still returned by
    /// `IDX.QUERY` twelve seconds later, hydrating to nil fields.
    ///
    /// Drained beside `drain_store_notify` because they are the same
    /// moment; kept separate because that one is observability and this
    /// one is correctness.
    pub(crate) fn drain_expired_keys(&mut self) {
        if !self.store.has_expired_keys() {
            return;
        }
        for key in self.store.take_expired_keys() {
            self.note_key_mutated(&key);
        }
    }

    pub(crate) fn drain_store_notify(&mut self) {
        if !self.store.has_notify_events() {
            return;
        }
        for (kind, key) in self.store.take_notify_events() {
            self.notify_keyspace_event(kind.name().as_bytes(), &key);
        }
    }

    /// `FLUSHDB` / `FLUSHALL` — fire one `flushdb` event on the event
    /// channel (no per-key keyspace channel since no specific key
    /// applies). Matches Redis events.c semantics.
    pub(crate) fn maybe_notify_flush(&mut self) {
        if !self
            .notify_flags
            .contains(crate::NotificationFlags::GENERIC | crate::NotificationFlags::KEYEVENT)
        {
            return;
        }
        // Just the event channel — no per-key keyspace channel applies.
        let mut chan = Vec::from(b"__keyevent@0__:flushdb".as_slice());
        let _ = &mut chan; // silence lint
        self.broadcast_notification(b"__keyevent@0__:flushdb", b"");
    }
}

/// In-place ASCII lowercase of a slice (the verb usually arrives
/// already-uppercased from clients like redis-cli; we lower so the
/// event name matches Redis's events.c convention).
fn ascii_lower(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for &b in s {
        out.push(b.to_ascii_lowercase());
    }
    out
}

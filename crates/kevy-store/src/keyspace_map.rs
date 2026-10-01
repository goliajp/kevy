//! The keyspace table, wrapped so that every mutable access first lets the
//! row recorder see the row (see `row_watch`).
//!
//! Reads go straight through ([`Deref`]). A mutable access comes in two
//! kinds: the plain one records the row before handing it out, and the
//! `quiet` one does not — for changes that leave the row's content as it
//! was (access clocks, weights, moving a value between hot and cold, or
//! between packed and general form). Recording is the default, so a write
//! path added later is recorded unless it opts out.

use core::ops::Deref;

use kevy_map::{KevyMap, RawEntryMut};

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::row_watch::Journal;
use crate::{Entry, SmallBytes};

#[derive(Debug, Default)]
pub(crate) struct Keyspace {
    map: KevyMap<SmallBytes, Entry>,
    pub(crate) rows: Option<Box<Journal>>,
}

impl Deref for Keyspace {
    type Target = KevyMap<SmallBytes, Entry>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

impl Keyspace {
    #[inline]
    fn note(&mut self, key: &[u8]) {
        if let Some(j) = self.rows.as_deref_mut() {
            j.note(key, self.map.get(key));
        }
    }

    /// The entry at `key`, for a write: the row is recorded first.
    #[inline]
    pub(crate) fn get_mut(&mut self, key: &[u8]) -> Option<&mut Entry> {
        self.note(key);
        self.map.get_mut(key)
    }

    /// The entry at `key`, for a change that keeps the row's content.
    #[inline]
    pub(crate) fn get_mut_quiet(&mut self, key: &[u8]) -> Option<&mut Entry> {
        self.map.get_mut(key)
    }

    /// The entry at `slot` (a [`KevyMap::find_slot`] result for `key`, with
    /// no insert or remove since), for a write: the row is recorded first.
    #[inline]
    pub(crate) fn entry_at_mut(&mut self, key: &[u8], slot: usize) -> Option<&mut Entry> {
        self.note(key);
        self.map.slot_mut(slot).map(|(_, e)| e)
    }

    /// [`Self::entry_at_mut`] for a change that keeps the row's content.
    #[inline]
    pub(crate) fn entry_at_quiet(&mut self, slot: usize) -> Option<&mut Entry> {
        self.map.slot_mut(slot).map(|(_, e)| e)
    }

    #[inline]
    pub(crate) fn insert(&mut self, key: SmallBytes, e: Entry) -> Option<Entry> {
        self.note(key.as_slice());
        self.map.insert(key, e)
    }

    /// Insert a row that already exists in another form (a cold row's
    /// stub), so nothing the row holds changes.
    #[cfg_attr(not(all(feature = "std", not(target_arch = "wasm32"))), allow(dead_code))]
    pub(crate) fn insert_quiet(&mut self, key: SmallBytes, e: Entry) -> Option<Entry> {
        self.map.insert(key, e)
    }

    #[inline]
    pub(crate) fn remove(&mut self, key: &[u8]) -> Option<Entry> {
        self.note(key);
        self.map.remove(key)
    }

    #[inline]
    pub(crate) fn raw_entry_mut(&mut self, key: &[u8]) -> RawEntryMut<'_, SmallBytes, Entry> {
        self.note(key);
        self.map.raw_entry_mut(key)
    }

    /// Wipe every key; the recorder notes that everything changed.
    pub(crate) fn clear(&mut self) {
        if let Some(j) = self.rows.as_deref_mut() {
            j.reset();
        }
        self.map.clear();
    }

    /// Take the whole table out (an asynchronous flush).
    pub(crate) fn detach(&mut self) -> KevyMap<SmallBytes, Entry> {
        if let Some(j) = self.rows.as_deref_mut() {
            j.reset();
        }
        core::mem::take(&mut self.map)
    }

    /// The table itself, for a pass that rewrites where values live
    /// without changing what they hold (cold-tier compaction).
    #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
    pub(crate) fn quiet_table(&mut self) -> &mut KevyMap<SmallBytes, Entry> {
        &mut self.map
    }
}

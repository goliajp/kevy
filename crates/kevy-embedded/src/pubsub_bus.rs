//! The in-process pub/sub bus — the per-`Inner` registry that maps
//! channel names + patterns to per-subscription `Sender`s. Extracted
//! from `pubsub.rs` to keep that file under the 500-LOC house rule;
//! the `Subscription` user-facing handle still lives in `pubsub.rs`.

use std::collections::HashMap;
use std::sync::mpsc::Sender;

use kevy_store::glob_match;

use crate::pubsub::PubsubFrame;

/// Internal entry in the bus tables.
struct BusEntry {
    id: u64,
    sender: Sender<PubsubFrame>,
}

/// The pub/sub registry, owned by `crate::store::Inner`.
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

    pub(crate) fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = id.wrapping_add(1).max(1);
        id
    }

    /// Total channels + patterns the given subscription id is bound to.
    pub(crate) fn count_for(&self, id: u64) -> usize {
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

    pub(crate) fn add_channel(
        &mut self,
        id: u64,
        sender: &Sender<PubsubFrame>,
        channel: Vec<u8>,
    ) -> bool {
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

    pub(crate) fn add_pattern(
        &mut self,
        id: u64,
        sender: &Sender<PubsubFrame>,
        pattern: Vec<u8>,
    ) -> bool {
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

    pub(crate) fn remove_channel(&mut self, id: u64, channel: &[u8]) -> bool {
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

    pub(crate) fn remove_pattern(&mut self, id: u64, pattern: &[u8]) -> bool {
        let before = self.patterns.len();
        self.patterns.retain(|(p, e)| !(p == pattern && e.id == id));
        self.patterns.len() < before
    }

    pub(crate) fn remove_all_for(&mut self, id: u64) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
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

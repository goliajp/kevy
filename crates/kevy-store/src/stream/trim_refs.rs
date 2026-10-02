//! Trims that look at what the consumer groups hold, as Redis 8.2's
//! `XADD` / `XTRIM` `KEEPREF | DELREF | ACKED` do.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;

use super::{StreamData, StreamId, TrimMode, TrimTo};

/// What a trim does about the entries consumer groups still reference.
///
/// ```
/// assert_eq!(kevy_store::TrimRefs::default(), kevy_store::TrimRefs::KeepRef);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TrimRefs {
    /// Trim; the groups keep their pending references to what went.
    ///
    /// ```
    /// assert_ne!(kevy_store::TrimRefs::KeepRef, kevy_store::TrimRefs::DelRef);
    /// ```
    #[default]
    KeepRef,
    /// Trim, and drop every group's pending reference to what went.
    ///
    /// ```
    /// assert_ne!(kevy_store::TrimRefs::DelRef, kevy_store::TrimRefs::Acked);
    /// ```
    DelRef,
    /// Remove only entries every group has read and acknowledged, oldest
    /// first, stepping over the others, until the stream is short enough
    /// (or its first remaining entry new enough).
    ///
    /// ```
    /// assert_ne!(kevy_store::TrimRefs::Acked, kevy_store::TrimRefs::KeepRef);
    /// ```
    Acked,
}

/// What a trim did.
///
/// ```
/// let t = kevy_store::Trimmed::default();
/// assert_eq!((t.removed, t.cut_at), (0, None));
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Trimmed {
    /// How many entries went.
    pub removed: u64,
    /// An `ACKED` approximate trim that its `LIMIT` stopped: the first ID
    /// of the node it stopped at. Every entry below it was looked at, none
    /// at or above it.
    pub cut_at: Option<StreamId>,
}

impl StreamData {
    /// Trim to `to` as `mode` says, treating group references as `refs`
    /// says. A `KEEPREF` approximate trim removes whole nodes only; a
    /// `DELREF` or `ACKED` one goes entry by entry, as Redis's do, its
    /// `LIMIT` checked at the start of each node.
    pub fn trim_refs(&mut self, to: TrimTo, mode: TrimMode, refs: TrimRefs) -> Trimmed {
        match refs {
            TrimRefs::KeepRef => Trimmed { removed: self.trim(to, mode) as u64, cut_at: None },
            TrimRefs::DelRef => {
                let mut gone = Vec::new();
                let done = self.trim_entries(to, mode, false, &mut gone);
                for (_, g) in self.groups.iter_mut() {
                    for id in &gone {
                        if let Some(p) = g.pel.remove(id)
                            && let Some(c) = g.consumers.get_mut(p.consumer.as_slice())
                        {
                            c.pel_count = c.pel_count.saturating_sub(1);
                        }
                    }
                }
                done
            }
            TrimRefs::Acked => self.trim_entries(to, mode, true, &mut Vec::new()),
        }
    }

    /// Whether every group has read `id` and none still holds it pending.
    fn acked_by_all(&self, id: StreamId) -> bool {
        self.groups.values().all(|g| id <= g.last_delivered_id && !g.pel.contains_key(&id))
    }

    /// Oldest first, until the stream is short enough (or its first
    /// remaining entry new enough), each entry removed — or with
    /// `acked_only`, each one every group has acknowledged, the others
    /// stepped over. The IDs that went are pushed to `gone`.
    fn trim_entries(
        &mut self,
        to: TrimTo,
        mode: TrimMode,
        acked_only: bool,
        gone: &mut Vec<StreamId>,
    ) -> Trimmed {
        let limit = match mode {
            TrimMode::Approximate { limit } => limit,
            TrimMode::Exact => 0,
        };
        let mut done = Trimmed::default();
        let mut node_end: Option<StreamId> = None;
        let mut next = self.entries.keys().next().copied();
        while let Some(id) = next {
            let enough = match to {
                TrimTo::MaxLen(n) => self.entries.len() as u64 <= n,
                TrimTo::MinId(min) => id >= min,
            };
            if enough {
                break;
            }
            // the limit is counted in whole nodes, at each node's start
            if node_end.is_none_or(|end| id > end) {
                let (first, last, live) = self.nodes.holding(id);
                if limit != 0 && done.removed as usize + live > limit {
                    done.cut_at = Some(first);
                    break;
                }
                node_end = Some(last);
            }
            next = self.entries.range(id.next()..).next().map(|(k, _)| *k);
            if !acked_only || self.acked_by_all(id) {
                self.entries.remove(&id);
                self.nodes.delete(id);
                gone.push(id);
                done.removed += 1;
            }
        }
        done
    }
}

impl crate::Store {
    /// `XTRIM key MAXLEN|MINID [=|~] threshold [LIMIT n] [KEEPREF | DELREF
    /// | ACKED]`: see [`StreamData::trim_refs`]. Nothing on a missing key.
    ///
    /// ```
    /// use kevy_store::{MissingStream, Store, StreamId, TrimMode, TrimRefs, TrimTo, XAddIdSpec};
    /// let mut s = Store::new();
    /// for ms in 1..=3 {
    ///     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    ///     s.xadd(b"s", XAddIdSpec::Explicit(StreamId::new(ms, 0)), f, MissingStream::Create, 0)?;
    /// }
    /// let t = s.xtrim_refs(b"s", TrimTo::MaxLen(1), TrimMode::Exact, TrimRefs::Acked)?;
    /// assert_eq!(t.removed, 2, "no group holds anything, so everything is acknowledged");
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn xtrim_refs(
        &mut self,
        key: &[u8],
        to: TrimTo,
        mode: TrimMode,
        refs: TrimRefs,
    ) -> Result<Trimmed, crate::StoreError> {
        let t = match self.stream_mut(key, false)? {
            Some(s) => s.trim_refs(to, mode, refs),
            None => return Ok(Trimmed::default()),
        };
        if t.removed > 0 {
            self.bump_if_watched(key);
            self.reweigh_entry(key);
        }
        Ok(t)
    }
}

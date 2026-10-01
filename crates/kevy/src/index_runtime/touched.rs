//! The rows this shard's indexes took in, kept for the views that read them.
//!
//! A materialized view is derived from its indexes, so it has to see every
//! row they see: the write hook's key, but also a row a snapshot load, an
//! expiry or a replicated resync changed without calling it. The drain that
//! brings the indexes up to date lists each such row here, and the views
//! read the list once the indexes have applied it.

use kevy_index::Segment;

use super::{BuildState, ShardIndex, ShardIndexes};
use crate::state::Ctx;

#[derive(Debug, Default)]
pub(super) struct Touched {
    /// Some view reads this shard's indexes; nothing is kept otherwise.
    on: bool,
    /// The keyspace was wiped before the rows listed.
    wiped: bool,
    keys: Vec<u8>,
    ends: Vec<usize>,
}

impl Touched {
    pub(super) fn set_on(&mut self, on: bool) {
        self.on = on;
        if !on {
            self.clear();
        }
    }

    pub(super) fn wipe(&mut self) {
        if self.on {
            self.clear();
            self.wiped = true;
        }
    }

    pub(super) fn push(&mut self, key: &[u8]) {
        if self.on {
            self.keys.extend_from_slice(key);
            self.ends.push(self.keys.len());
        }
    }

    fn clear(&mut self) {
        self.wiped = false;
        self.keys.clear();
        self.ends.clear();
    }
}

/// This shard's indexes as a view reads them.
pub(crate) struct Rows<'s> {
    st: &'s ShardIndexes,
}

impl<'s> Rows<'s> {
    /// Moves whenever this shard's index list changes.
    pub(crate) fn generation(&self) -> u64 {
        self.st.generation
    }

    /// The keyspace was wiped since the last read: every row listed came
    /// after it.
    pub(crate) fn wiped(&self) -> bool {
        self.st.touched.wiped
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &'s [u8]> {
        let t = &self.st.touched;
        let starts = std::iter::once(0).chain(t.ends.iter().copied());
        starts.zip(t.ends.iter().copied()).map(|(a, b)| &t.keys[a..b])
    }

    /// The build of `name` on this shard, 0 when it has none: a view derived
    /// from one build is stale once another replaces it.
    pub(crate) fn build_of(&self, name: &[u8]) -> u64 {
        self.st.idx.iter().find(|si| si.spec.name() == name).map_or(0, |si| si.build_id)
    }

    pub(crate) fn ready(&self, name: &[u8]) -> Option<&'s Segment> {
        ready(&self.st.idx, name)
    }
}

/// `name`'s segment when it is ready and holds this shard's own rows (a
/// global index's entries are not: it resolves to nothing, and a view
/// refuses it).
pub(super) fn ready<'s>(idx: &'s [ShardIndex], name: &[u8]) -> Option<&'s Segment> {
    idx.iter()
        .find(|si| si.spec.name() == name && matches!(si.build, BuildState::Ready))
        .filter(|si| si.global.is_none())
        .map(|si| &si.seg)
}

/// Run `f` over the rows taken in since the last call; they are then gone.
pub(crate) fn with_rows<R>(ctx: &Ctx<'_>, f: impl FnOnce(&Rows<'_>) -> R) -> R {
    let mut st = ctx.shard.indexes.borrow_mut();
    super::refresh(ctx, &mut st);
    let out = f(&Rows { st: &st });
    st.touched.clear();
    out
}

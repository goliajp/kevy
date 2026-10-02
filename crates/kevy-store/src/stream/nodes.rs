//! Where a stream's entries would sit in the nodes a Redis server keeps
//! them in, so an approximate trim (`MAXLEN ~` / `MINID ~`) removes what
//! that server removes: whole nodes from the head, never part of one.
//!
//! A node takes entries until it holds 100 of them, deleted ones included,
//! or until the raw bytes of the next entry's fields and values would
//! bring its encoded size to 4096. The encoded size follows the listpack
//! layout: a header naming the first entry's fields, then per entry a
//! flag, the ID as a difference from the node's first, the values (and the
//! field names when they differ from the first entry's), and an element
//! count; integers and short strings take fewer bytes. The sizes were
//! measured against a valkey 9.1 server entry shape by entry shape.
//! Deleting an entry frees no bytes; deleting a node's last live entry
//! frees the node.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use alloc::collections::VecDeque;

use super::{StreamData, StreamId};
use crate::value::SmallBytes;

const NODE_MAX_ENTRIES: u32 = 100;
const NODE_MAX_BYTES: usize = 4096;

/// How many entries an approximate trim removes at most when the command
/// names no `LIMIT`: 100 nodes' worth.
///
/// ```
/// assert_eq!(kevy_store::APPROX_TRIM_LIMIT, 10_000);
/// ```
pub const APPROX_TRIM_LIMIT: usize = 100 * NODE_MAX_ENTRIES as usize;

/// How a trim goes about it: `XTRIM`'s `=` and `~`.
///
/// ```
/// use kevy_store::{APPROX_TRIM_LIMIT, TrimMode};
/// assert_ne!(TrimMode::Exact, TrimMode::Approximate { limit: APPROX_TRIM_LIMIT });
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TrimMode {
    /// Entry by entry until what the trim keeps is all that is left.
    ///
    /// ```
    /// assert_eq!(kevy_store::TrimMode::Exact, kevy_store::TrimMode::Exact);
    /// ```
    Exact,
    /// Whole nodes from the head only, and no more than `limit` entries
    /// (0 for no limit).
    ///
    /// ```
    /// let m = kevy_store::TrimMode::Approximate { limit: 0 };
    /// assert!(matches!(m, kevy_store::TrimMode::Approximate { limit: 0 }));
    /// ```
    Approximate {
        /// Entries the trim may remove, 0 for no limit.
        ///
        /// ```
        /// let mode = kevy_store::TrimMode::Approximate { limit: 7 };
        /// assert!(matches!(mode, kevy_store::TrimMode::Approximate { limit: 7 }));
        /// ```
        limit: usize,
    },
}

/// What a trim keeps: the last `n` entries, or the entries from an ID on.
///
/// ```
/// use kevy_store::{StreamId, TrimTo};
/// assert_ne!(TrimTo::MaxLen(5), TrimTo::MinId(StreamId::new(5, 0)));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TrimTo {
    /// Keep at most this many entries.
    ///
    /// ```
    /// assert_eq!(kevy_store::TrimTo::MaxLen(3), kevy_store::TrimTo::MaxLen(3));
    /// ```
    MaxLen(u64),
    /// Keep the entries whose ID is not below this one.
    ///
    /// ```
    /// use kevy_store::{StreamId, TrimTo};
    /// assert!(matches!(TrimTo::MinId(StreamId::MIN), TrimTo::MinId(_)));
    /// ```
    MinId(StreamId),
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Nodes {
    list: VecDeque<Node>,
}

#[derive(Debug, Clone)]
struct Node {
    first: StreamId,
    last: StreamId,
    fields: Box<[SmallBytes]>,
    appended: u32,
    live: u32,
    bytes: usize,
}

impl Nodes {
    pub(super) fn len(&self) -> usize {
        self.list.len()
    }

    /// Place the entry `id` just appended at the tail.
    pub(super) fn append(&mut self, id: StreamId, fv: &[(SmallBytes, SmallBytes)]) {
        let raw: usize = fv.iter().map(|(f, v)| f.len() + v.len()).sum();
        let fits = self
            .list
            .back()
            .is_some_and(|n| n.appended < NODE_MAX_ENTRIES && n.bytes + raw < NODE_MAX_BYTES);
        if !fits {
            let fields: Box<[SmallBytes]> = fv.iter().map(|(f, _)| f.clone()).collect();
            let bytes = header_bytes(&fields);
            self.list.push_back(Node { first: id, last: id, fields, appended: 0, live: 0, bytes });
        }
        let Some(n) = self.list.back_mut() else { return };
        n.bytes += entry_bytes(n, id, fv);
        n.appended += 1;
        n.live += 1;
        n.last = id;
    }

    /// `(first, last, live entries)` of the node holding `id`.
    pub(super) fn holding(&self, id: StreamId) -> (StreamId, StreamId, usize) {
        let at = self.list.partition_point(|n| n.first <= id);
        at.checked_sub(1).map_or((id, id, 1), |i| {
            let n = &self.list[i];
            (n.first, n.last, n.live as usize)
        })
    }

    /// The entry `id`, which the stream held, is gone.
    pub(super) fn delete(&mut self, id: StreamId) {
        let at = self.list.partition_point(|n| n.first <= id);
        let Some(i) = at.checked_sub(1) else { return };
        let n = &mut self.list[i];
        n.live = n.live.saturating_sub(1);
        if n.live == 0 {
            self.list.remove(i);
        }
    }
}

impl StreamData {
    /// How many nodes a Redis server would hold these entries in:
    /// `XINFO STREAM`'s `radix-tree-keys`.
    ///
    /// ```
    /// use kevy_store::{MissingStream, Store, StreamId, XAddIdSpec};
    /// let mut s = Store::new();
    /// for ms in 1..=101 {
    ///     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    ///     s.xadd(b"s", XAddIdSpec::Explicit(StreamId::new(ms, 0)), f, MissingStream::Create, 0)?;
    /// }
    /// assert_eq!(s.stream_view(b"s")?.unwrap().node_count(), 2, "100 entries to a node");
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// `XINFO STREAM`'s `radix-tree-nodes`: the nodes of a radix tree
    /// over the stream nodes' first IDs, each 16 bytes big-endian, with a
    /// shared run of bytes held in one node, a fork in one node, and a
    /// node at the end of each key. Measured against a valkey 9.1 server.
    ///
    /// ```
    /// use kevy_store::{MissingStream, Store, StreamId, XAddIdSpec};
    /// let mut s = Store::new();
    /// for ms in 1..=250 {
    ///     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    ///     s.xadd(b"s", XAddIdSpec::Explicit(StreamId::new(ms, 0)), f, MissingStream::Create, 0)?;
    /// }
    /// assert_eq!(s.stream_view(b"s")?.unwrap().radix_tree_nodes(), 8);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn radix_tree_nodes(&self) -> usize {
        let keys: Vec<[u8; 16]> = self.nodes.list.iter().map(|n| key_bytes(n.first)).collect();
        if keys.is_empty() { 1 } else { radix_subtree(&keys, 0) }
    }

    /// Trim to `to` as `mode` says, returning how many entries went. A trim
    /// leaves `max_deleted_id` alone: that marks a hole a deletion made
    /// among the entries, which a trim from the head never does.
    pub fn trim(&mut self, to: TrimTo, mode: TrimMode) -> usize {
        self.trim_each(to, mode, &mut |_| {})
    }

    /// [`Self::trim`], handing each ID that goes to `gone`.
    pub(super) fn trim_each(
        &mut self,
        to: TrimTo,
        mode: TrimMode,
        gone: &mut impl FnMut(StreamId),
    ) -> usize {
        let limit = match mode {
            TrimMode::Approximate { limit } => limit,
            TrimMode::Exact => 0,
        };
        let mut removed = 0usize;
        while let Some(head) = self.nodes.list.front() {
            let whole = match to {
                TrimTo::MaxLen(n) => (self.entries.len() - head.live as usize) as u64 >= n,
                TrimTo::MinId(min) => head.last < min,
            };
            if !whole || (limit != 0 && removed + head.live as usize > limit) {
                break;
            }
            let last = head.last;
            while let Some((&id, _)) = self.entries.first_key_value() {
                if id > last {
                    break;
                }
                self.entries.pop_first();
                gone(id);
                removed += 1;
            }
            self.nodes.list.pop_front();
        }
        if mode != TrimMode::Exact {
            return removed;
        }
        while let Some((&first, _)) = self.entries.first_key_value() {
            let over = match to {
                TrimTo::MaxLen(n) => self.entries.len() as u64 > n,
                TrimTo::MinId(min) => first < min,
            };
            if !over {
                break;
            }
            self.entries.pop_first();
            self.nodes.delete(first);
            gone(first);
            removed += 1;
        }
        removed
    }
}

impl crate::Store {
    /// `XTRIM key MAXLEN|MINID [=|~] threshold [LIMIT n]`: see
    /// [`StreamData::trim`]. Returns how many entries went; 0 on a missing
    /// key.
    ///
    /// ```
    /// use kevy_store::{MissingStream, Store, StreamId, TrimMode, TrimTo, XAddIdSpec};
    /// let mut s = Store::new();
    /// for ms in 1..=150 {
    ///     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    ///     s.xadd(b"s", XAddIdSpec::Explicit(StreamId::new(ms, 0)), f, MissingStream::Create, 0)?;
    /// }
    /// let nodes = TrimMode::Approximate { limit: 0 };
    /// assert_eq!(s.xtrim(b"s", TrimTo::MaxLen(120), nodes)?, 0, "the head node holds 100");
    /// assert_eq!(s.xtrim(b"s", TrimTo::MaxLen(120), TrimMode::Exact)?, 30);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn xtrim(
        &mut self,
        key: &[u8],
        to: TrimTo,
        mode: TrimMode,
    ) -> Result<u64, crate::StoreError> {
        let n = match self.stream_mut(key, false)? {
            Some(s) => s.trim(to, mode),
            None => return Ok(0),
        };
        if n > 0 {
            self.bump_if_watched(key);
            self.reweigh_entry(key);
        }
        Ok(n as u64)
    }
}

fn key_bytes(id: StreamId) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[..8].copy_from_slice(&id.ms.to_be_bytes());
    k[8..].copy_from_slice(&id.seq.to_be_bytes());
    k
}

/// Nodes under the point `depth` bytes into `keys`, which are sorted,
/// distinct, and share their first `depth` bytes.
fn radix_subtree(keys: &[[u8; 16]], depth: usize) -> usize {
    if keys.len() == 1 {
        return if depth < 16 { 2 } else { 1 };
    }
    let (first, last) = (&keys[0], &keys[keys.len() - 1]);
    let shared = (depth..16).take_while(|&i| first[i] == last[i]).count();
    let fork = depth + shared;
    let mut n = usize::from(shared > 0) + 1;
    let mut from = 0;
    while from < keys.len() {
        let b = keys[from][fork];
        let to = from + keys[from..].iter().take_while(|k| k[fork] == b).count();
        n += radix_subtree(&keys[from..to], fork + 1);
        from = to;
    }
    n
}

/// A node's fixed part: the listpack header and end byte, the valid and
/// deleted counts, the first entry's field names, and their terminator.
fn header_bytes(fields: &[SmallBytes]) -> usize {
    let names: usize = fields.iter().map(|f| elem(f.as_slice())).sum();
    7 + int_elem(0) * 2 + int_elem(fields.len() as i128) + names + int_elem(0)
}

fn entry_bytes(n: &Node, id: StreamId, fv: &[(SmallBytes, SmallBytes)]) -> usize {
    let same =
        fv.len() == n.fields.len() && fv.iter().zip(n.fields.iter()).all(|((f, _), m)| f == m);
    let ms = i128::from(id.ms) - i128::from(n.first.ms);
    let seq = i128::from(id.seq) - i128::from(n.first.seq);
    let values: usize = fv.iter().map(|(_, v)| elem(v.as_slice())).sum();
    let count = fv.len() as i128;
    let (names, parts) = if same {
        (0, count + 3)
    } else {
        let names: usize = fv.iter().map(|(f, _)| elem(f.as_slice())).sum();
        (int_elem(count) + names, 2 * count + 4)
    };
    int_elem(2) + int_elem(ms) + int_elem(seq) + names + values + int_elem(parts)
}

/// One element holding `b`: as an integer when it reads as one.
fn elem(b: &[u8]) -> usize {
    if let Some(v) = as_int(b) {
        return int_elem(i128::from(v));
    }
    let head = match b.len() {
        0..=63 => 1,
        64..=4095 => 2,
        _ => 5,
    };
    with_backlen(head + b.len())
}

fn int_elem(v: i128) -> usize {
    let size = match v {
        0..=127 => 1,
        -4096..=4095 => 2,
        -32768..=32767 => 3,
        -8_388_608..=8_388_607 => 4,
        -2_147_483_648..=2_147_483_647 => 5,
        _ => 9,
    };
    with_backlen(size)
}

fn with_backlen(size: usize) -> usize {
    size + match size {
        0..=127 => 1,
        128..=16_383 => 2,
        16_384..=2_097_151 => 3,
        2_097_152..=268_435_455 => 4,
        _ => 5,
    }
}

/// `b` as the integer it spells, when it spells one the way an integer
/// prints: no sign but a leading `-`, no leading zero, at most 20 bytes.
fn as_int(b: &[u8]) -> Option<i64> {
    let digits = b.strip_prefix(b"-").unwrap_or(b);
    let canonical = !digits.is_empty()
        && b.len() <= 20
        && digits.iter().all(u8::is_ascii_digit)
        && (digits[0] != b'0' || b == b"0");
    if !canonical {
        return None;
    }
    core::str::from_utf8(b).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(n: u64, fields: impl Fn(u64) -> Vec<(&'static [u8], Vec<u8>)>) -> StreamData {
        let mut s = StreamData::default();
        for i in 1..=n {
            let fv = fields(i)
                .into_iter()
                .map(|(f, v)| (SmallBytes::from_slice(f), SmallBytes::from_vec(v)))
                .collect();
            s.insert(StreamId::new(i, 0), fv);
        }
        s
    }

    /// Entries in each node, as a valkey 9.1 server laid the same streams
    /// out (read back from DUMP).
    fn per_node(s: &StreamData) -> Vec<u32> {
        s.nodes.list.iter().map(|n| n.appended).collect()
    }

    #[test]
    fn a_node_holds_what_the_measured_server_puts_in_one() {
        for (vlen, per) in [(30, 100), (38, 85), (39, 83), (51, 66), (63, 55), (64, 54), (500, 7)] {
            let s = stream(300, |_| vec![(&b"f"[..], vec![b'x'; vlen])]);
            assert_eq!(per_node(&s)[0], per, "values of {vlen} bytes");
        }
        let two = stream(300, |_| vec![(&b"f"[..], vec![b'x'; 30]), (&b"g"[..], vec![b'y'; 30])]);
        assert_eq!(per_node(&two)[0], 56);
        let alt =
            stream(300, |i| vec![(if i % 3 == 0 { &b"b"[..] } else { &b"a"[..] }, vec![b'x'; 30])]);
        assert_eq!(per_node(&alt)[0], 98);
    }

    #[test]
    fn the_id_difference_counts_as_the_integer_it_is() {
        for (spacing, per) in [(100u64, 98), (128, 98), (1000, 96), (32768, 95), (1 << 40, 85)] {
            let mut s = StreamData::default();
            for k in 0..300u64 {
                let fv = vec![(SmallBytes::from_slice(b"f"), SmallBytes::from_vec(vec![b'x'; 30]))];
                s.insert(StreamId::new(1 + spacing * k, 0), fv);
            }
            assert_eq!(per_node(&s)[0], per, "ms spacing {spacing}");
        }
    }

    #[test]
    fn integers_are_stored_as_integers() {
        assert_eq!(as_int(b"1000000000000000"), Some(1_000_000_000_000_000));
        assert_eq!(as_int(b"-7"), Some(-7));
        for not in [&b"01"[..], b"-0", b"+1", b"", b"-", b"1a", b"99999999999999999999"] {
            assert_eq!(as_int(not), None, "{not:?}");
        }
    }

    #[test]
    fn an_approximate_trim_takes_whole_nodes() {
        let mut s = stream(251, |_| vec![(&b"f"[..], b"v".to_vec())]);
        assert_eq!(
            s.trim(TrimTo::MaxLen(120), TrimMode::Approximate { limit: APPROX_TRIM_LIMIT }),
            100
        );
        assert_eq!(
            s.trim(TrimTo::MaxLen(10), TrimMode::Approximate { limit: APPROX_TRIM_LIMIT }),
            100
        );
        assert_eq!(s.length(), 51);
        assert_eq!(s.trim(TrimTo::MaxLen(0), TrimMode::Approximate { limit: 0 }), 51);
        let mut t = stream(350, |_| vec![(&b"f"[..], b"v".to_vec())]);
        assert_eq!(
            t.trim(TrimTo::MaxLen(10), TrimMode::Approximate { limit: 150 }),
            100,
            "a second node would pass the limit"
        );
        let min = |ms| TrimTo::MinId(StreamId::new(ms, 0));
        assert_eq!(
            t.trim(min(150), TrimMode::Approximate { limit: 0 }),
            0,
            "the head node, 101..=200, holds 150 and on"
        );
        assert_eq!(t.trim(min(201), TrimMode::Approximate { limit: 0 }), 100);
    }

    #[test]
    fn an_exact_trim_and_deletions_leave_the_node_and_its_count() {
        let mut s = stream(250, |_| vec![(&b"f"[..], b"v".to_vec())]);
        assert_eq!(s.trim(TrimTo::MaxLen(240), TrimMode::Exact), 10);
        assert_eq!(
            s.trim(TrimTo::MaxLen(150), TrimMode::Approximate { limit: 0 }),
            90,
            "the head node held 90"
        );
        let mut d = stream(50, |_| vec![(&b"f"[..], b"v".to_vec())]);
        let gone: Vec<StreamId> = (1..=10).map(|i| StreamId::new(i, 0)).collect();
        d.del_ids(&gone);
        for i in 51..=110 {
            d.insert(
                StreamId::new(i, 0),
                vec![(SmallBytes::from_slice(b"f"), SmallBytes::from_slice(b"v"))],
            );
        }
        assert_eq!(per_node(&d), [100, 10], "deleted entries still count towards a node's 100");
        let tail: Vec<StreamId> = (101..=110).map(|i| StreamId::new(i, 0)).collect();
        d.del_ids(&tail);
        assert_eq!(d.node_count(), 1, "a node whose entries are all gone is freed");
    }
}

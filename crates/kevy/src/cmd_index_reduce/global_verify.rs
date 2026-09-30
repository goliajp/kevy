//! `IDX.VERIFY` on a global index, the origin's half: match the entries
//! the owners hold against the entries the rows call for, by key.
//!
//! An entry held with no row calling for it, or held in another partition
//! or with another hash than its row now calls for, is `drift`; a row
//! whose entry no owner holds is `missing`; `checked` counts the entries
//! held. Both lists cover every shard, so the counts are exact.

use std::collections::HashMap;

use kevy_resp::{encode_array_len, encode_bulk};

use crate::index_runtime::{Placed, VERIFY_TAG};

/// The reply, or `None` when the chunks are not a global index's.
pub(super) fn reduce(chunks: &[Vec<u8>]) -> Option<Vec<u8>> {
    if chunks.is_empty() || !chunks.iter().all(|c| c.get(1) == Some(&VERIFY_TAG)) {
        return None;
    }
    let (mut sums, mut held, mut owed) = ([0u64; 4], Vec::new(), Vec::new());
    for c in chunks {
        let mut r = Reader(c.get(2..)?);
        for s in &mut sums {
            *s += r.u64()?;
        }
        held.extend(r.list()?);
        owed.extend(r.list()?);
    }
    let [entries, bytes, coerce_failures, duplicates] = sums;
    let (drift, missing) = reconcile(&held, &owed);
    let counts = [
        (&b"entries"[..], entries),
        (b"bytes", bytes),
        (b"coerce_failures", coerce_failures),
        (b"duplicates", duplicates),
        (b"drift", drift),
        (b"checked", held.len() as u64),
        (b"missing", missing),
    ];
    let mut out = Vec::new();
    encode_array_len(&mut out, 2 * counts.len() as i64);
    for (label, n) in counts {
        encode_bulk(&mut out, label);
        encode_bulk(&mut out, n.to_string().as_bytes());
    }
    Some(out)
}

/// `(drift, missing)`: held entries that disagree with their row or have
/// none, and rows whose entry nobody holds.
fn reconcile(held: &[Placed], owed: &[Placed]) -> (u64, u64) {
    let mut by_key: HashMap<&[u8], (u16, u64, bool)> =
        held.iter().map(|(k, p, h)| (k.as_slice(), (*p, *h, false))).collect();
    let (mut drift, mut missing) = (0, 0);
    for (k, p, h) in owed {
        match by_key.get_mut(k.as_slice()) {
            Some(e) => {
                e.2 = true;
                drift += u64::from((e.0, e.1) != (*p, *h));
            }
            None => missing += 1,
        }
    }
    drift += by_key.values().filter(|e| !e.2).count() as u64;
    (drift, missing)
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }

    fn u64(&mut self) -> Option<u64> {
        self.take().map(u64::from_le_bytes)
    }

    fn list(&mut self) -> Option<Vec<Placed>> {
        let n = u32::from_le_bytes(self.take()?);
        let mut out = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let len = u32::from_le_bytes(self.take()?) as usize;
            let (key, rest) = self.0.split_at_checked(len)?;
            self.0 = rest;
            let p = u16::from_le_bytes(self.take()?);
            out.push((key.to_vec(), p, self.u64()?));
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(k: &str, p: u16, h: u64) -> Placed {
        (k.as_bytes().to_vec(), p, h)
    }

    #[test]
    fn each_way_an_entry_and_its_row_can_disagree_is_counted_once() {
        let held = [e("a", 0, 1), e("b", 0, 2), e("c", 1, 3), e("stale", 1, 4)];
        let owed = [
            e("a", 0, 1),      // agrees
            e("b", 0, 9),      // row changed, entry did not
            e("c", 0, 3),      // row moved partition, entry stayed
            e("unheld", 1, 5), // no owner holds it
        ];
        // b and c disagree, stale has no row; unheld is owed and missing
        assert_eq!(reconcile(&held, &owed), (3, 1));
        assert_eq!(reconcile(&held, &held), (0, 0));
    }

    fn put(c: &mut Vec<u8>, list: &[Placed]) {
        c.extend_from_slice(&(list.len() as u32).to_le_bytes());
        for (k, p, h) in list {
            c.extend_from_slice(&(k.len() as u32).to_le_bytes());
            c.extend_from_slice(k);
            c.extend_from_slice(&p.to_le_bytes());
            c.extend_from_slice(&h.to_le_bytes());
        }
    }

    #[test]
    fn a_chunk_cut_at_any_byte_is_not_read_as_a_global_verify() {
        let mut c = vec![0, VERIFY_TAG];
        for n in [2u64, 64, 0, 0] {
            c.extend_from_slice(&n.to_le_bytes());
        }
        put(&mut c, &[e("a", 0, 1), e("b", 0, 2)]);
        put(&mut c, &[e("a", 0, 1)]);
        let whole = String::from_utf8(reduce(std::slice::from_ref(&c)).unwrap()).unwrap();
        assert!(whole.contains("$5\r\ndrift\r\n$1\r\n1\r\n"), "{whole}");
        for cut in 2..c.len() {
            assert_eq!(reduce(&[c[..cut].to_vec()]), None, "cut at {cut}");
        }
    }
}

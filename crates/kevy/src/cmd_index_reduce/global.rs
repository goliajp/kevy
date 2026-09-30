//! The origin's half of a global index's in-order page: the partitions
//! answer one after another, so each phase's hits follow the previous
//! phase's in `(value, key)` order and the page is their concatenation.
//! A phase that leaves the page short asks the next partition for the
//! rest; the page so far rides in the continuation argv.

use kevy_rt::ExtensionReduced;

use crate::cmd_index_query::{PART_ORIG, PART_VERB, REBUILD_TAG, global_walk};
use crate::state::RuntimeState;

/// The first phase of an `IDX.QUERY` on a global index: `None` unless it
/// is a page in order that went to one partition.
pub(super) fn first_phase(
    state: &RuntimeState,
    argv: &[Vec<u8>],
    chunks: &[Vec<u8>],
) -> Option<ExtensionReduced> {
    let (w, _) = global_walk(&state.catalogs, argv)?;
    let [chunk] = chunks else { return None };
    (w.in_order && argv[0].eq_ignore_ascii_case(b"IDX.QUERY"))
        .then(|| step(argv, w.start, w.last, w.limit, &[], chunk))
}

/// A continuation phase (`IDX.PART`). The index may have gone or changed
/// between phases; the page then ends with what it has.
pub(super) fn next_phase(
    state: &RuntimeState,
    argv: &[Vec<u8>],
    chunks: &[Vec<u8>],
) -> ExtensionReduced {
    let orig = &argv[PART_ORIG..];
    let carried = &argv[3];
    let p = std::str::from_utf8(&argv[1]).ok().and_then(|s| s.parse::<usize>().ok());
    match (global_walk(&state.catalogs, orig), p, chunks) {
        (Some((w, _)), Some(p), [chunk]) => step(orig, p, w.last, w.limit, carried, chunk),
        _ => {
            let page = chunks.iter().fold(carried.clone(), |acc, c| concat(&acc, c));
            ExtensionReduced::Reply(super::query::reduce_query(orig, &[page]))
        }
    }
}

/// Append partition `p`'s hits to the page; ask partition `p + 1` for the
/// rest while the page is short and the range goes on.
fn step(
    orig: &[Vec<u8>],
    p: usize,
    last: usize,
    limit: usize,
    carried: &[u8],
    chunk: &[u8],
) -> ExtensionReduced {
    let page = concat(carried, chunk);
    let got = hit_count(&page);
    if got < limit && p < last {
        let mut next = vec![
            PART_VERB.to_vec(),
            (p + 1).to_string().into_bytes(),
            (limit - got).to_string().into_bytes(),
            page,
        ];
        next.extend_from_slice(orig);
        return ExtensionReduced::Continue(next);
    }
    ExtensionReduced::Reply(super::query::reduce_query(orig, &[page]))
}

/// `IDX.REBUILD` on a global index: new split points from every shard's
/// rank buckets, installed as a new incarnation — every shard then sends its rows'
/// entries again, and the index answers once they all have. `None` when
/// the chunks are not a global rebuild's.
pub(super) fn rebuild(
    state: &RuntimeState,
    argv: &[Vec<u8>],
    chunks: &[Vec<u8>],
) -> Option<Vec<u8>> {
    if !chunks.iter().all(|c| c.get(1) == Some(&REBUILD_TAG)) {
        return None;
    }
    let name = argv.get(1)?;
    let mut points = Vec::new();
    for c in chunks {
        points.extend(crate::index_runtime::read_points(c, &mut 2)?);
    }
    let splits = kevy_index::splits_from_weighted(points, state.nshards().max(1));
    let mut cat = (*state.catalogs.index()?).clone();
    if !cat.set_splits(name, splits) {
        return None;
    }
    state.install_index_catalog(cat);
    Some(b"+OK\r\n".to_vec())
}

/// Two plain hit chunks (`[status][n u32][rows…]`) as one; an empty
/// `carried` is the first phase.
fn concat(carried: &[u8], chunk: &[u8]) -> Vec<u8> {
    if carried.is_empty() {
        return chunk.to_vec();
    }
    let n = hit_count(carried) + hit_count(chunk);
    let mut out = Vec::with_capacity(carried.len() + chunk.len());
    out.push(carried[0]);
    out.extend_from_slice(&(n as u32).to_le_bytes());
    out.extend_from_slice(carried.get(5..).unwrap_or_default());
    out.extend_from_slice(chunk.get(5..).unwrap_or_default());
    out
}

fn hit_count(chunk: &[u8]) -> usize {
    chunk.get(1..5).map_or(0, |b| u32::from_le_bytes(b.try_into().expect("four bytes")) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(rows: &[&[u8]]) -> Vec<u8> {
        let mut c = vec![0];
        c.extend_from_slice(&(rows.len() as u32).to_le_bytes());
        for r in rows {
            c.extend_from_slice(r);
        }
        c
    }

    #[test]
    fn a_short_page_asks_the_next_partition_for_what_it_still_wants() {
        let orig: Vec<Vec<u8>> = [&b"IDX.QUERY"[..], b"g", b"RANGE", b"0", b"9", b"LIMIT", b"5"]
            .iter()
            .map(|a| a.to_vec())
            .collect();
        let ExtensionReduced::Continue(next) = step(&orig, 1, 3, 5, &[], &chunk(&[b"a", b"b"]))
        else {
            panic!("a page of 2 out of 5 with partitions left must continue");
        };
        assert_eq!(&next[..3], &[b"IDX.PART".to_vec(), b"2".to_vec(), b"3".to_vec()]);
        assert_eq!(next[3], chunk(&[b"a", b"b"]));
        assert_eq!(&next[PART_ORIG..], &orig[..]);
        // the last partition ends the page however short it is
        assert!(matches!(
            step(&orig, 3, 3, 5, &next[3], &chunk(&[b"c"])),
            ExtensionReduced::Reply(_)
        ));
    }

    #[test]
    fn pages_concatenate_in_phase_order() {
        let page = concat(&chunk(&[b"a", b"b"]), &chunk(&[b"c"]));
        assert_eq!(page, chunk(&[b"a", b"b", b"c"]));
        assert_eq!(concat(&[], &chunk(&[b"x"])), chunk(&[b"x"]));
    }
}

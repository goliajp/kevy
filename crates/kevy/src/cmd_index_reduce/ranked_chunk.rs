//! Decoding one shard's ranked chunk. Split from `ranked.rs` for the
//! 500-LOC house rule (a `#[path]` child, so it shares the reduce's wire
//! helpers).

use super::{Bucket, Hit, read_highlight, read_hydration, read_kbytes, read_u32};

/// into `all`; a short/corrupt chunk stops that shard's contribution.
pub(super) fn collect_hits(
    c: &[u8],
    highlight: bool,
    sorted: bool,
    grouped: bool,
    all: &mut Vec<Hit>,
) -> usize {
    let mut pos = 1usize;
    let Some(n) = read_u32(c, &mut pos) else { return pos };
    for _ in 0..n {
        let Some(key) = read_kbytes(c, &mut pos) else { break };
        let Some(sb) = c.get(pos..pos + 8) else { break };
        let v = f64::from_le_bytes(sb.try_into().expect("8 bytes"));
        pos += 8;
        let Some(fv) = read_hydration(c, &mut pos) else { break };
        let hl = if highlight {
            match read_highlight(c, &mut pos) {
                Some(h) => h,
                None => break,
            }
        } else {
            Vec::new()
        };
        let okey = if sorted {
            match read_okey(c, &mut pos) {
                Some(k) => k,
                None => break,
            }
        } else {
            None
        };
        let dkey = if grouped {
            match read_okey(c, &mut pos) {
                Some(k) => k,
                None => break,
            }
        } else {
            None
        };
        all.push(Hit { score: v, key, fields: fv, spans: hl, okey, dkey });
    }
    pos
}

/// The facet block that follows the hits: per requested field, its
/// buckets as `(identity, label, count)`.
///
/// Summed by identity, not by label: two shards can spell the same value
/// differently (`1` and `1.0` in a field declared `f64`) and those are
/// one bucket. The label reported is the first spelling seen, which is
/// therefore always one that occurs in the corpus.
pub(super) fn collect_facets(c: &[u8], mut pos: usize, n_fields: usize, out: &mut [Vec<Bucket>]) {
    for field in out.iter_mut().take(n_fields) {
        let Some(n) = read_u32(c, &mut pos) else { return };
        for _ in 0..n {
            let Some(key) = read_kbytes(c, &mut pos) else { return };
            let Some(label) = read_kbytes(c, &mut pos) else { return };
            let Some(cb) = c.get(pos..pos + 8) else { return };
            let count = u64::from_le_bytes(cb.try_into().expect("8 bytes"));
            pos += 8;
            match field.iter_mut().find(|(k, _, _)| *k == key) {
                Some(e) => e.2 += count,
                None => field.push((key, label, count)),
            }
        }
    }
}

/// value at all.
fn read_okey(c: &[u8], pos: &mut usize) -> Option<Option<Vec<u8>>> {
    let tag = *c.get(*pos)?;
    *pos += 1;
    match tag {
        0 => Some(None),
        1 => {
            let n = read_u32(c, pos)? as usize;
            let bytes = c.get(*pos..*pos + n)?.to_vec();
            *pos += n;
            Some(Some(bytes))
        }
        _ => None,
    }
}

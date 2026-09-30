//! What a shard contributes when a global index's split points are taken
//! from the data: its rows' index values in order, cut by rank into
//! buckets of equal size, each sent as its largest value and how many rows
//! it stands for. Split points drawn from these are off by at most one
//! bucket per shard — `1 / POINTS_PER_PARTITION` of a partition in all —
//! where a sample of rows is off by its sampling error, which is about
//! `1 / sqrt(sample)` per partition and larger for the largest of many.
//!
//! Wire form: `n u32 | n × (weight u64 | len u32 | value)`.

use kevy_index::IndexSpec;
use kevy_store::Store;

/// Buckets a shard sends per partition.
pub(crate) const POINTS_PER_PARTITION: usize = 256;

/// This shard's rows' encoded values for `spec`, in `points` buckets of
/// equal rank: each bucket's largest value and its row count. A shard with
/// no more rows than `points` sends every value with weight 1.
pub(crate) fn quantile_points(
    store: &mut Store,
    spec: &IndexSpec,
    points: usize,
) -> Vec<(Vec<u8>, u64)> {
    // one buffer for every value: a Vec per row would cost more than the
    // index itself does per row; the keys are walked, not copied
    let (mut buf, mut ends) = (Vec::new(), Vec::new());
    let mut walk = crate::key_walk::KeyWalk::new(spec.prefix());
    while !walk.is_done() {
        for k in walk.next_batch(store, 1024) {
            if let Some((v, _)) = super::global::derive(store, spec, &k) {
                buf.extend_from_slice(&v.order_bytes());
                ends.push(buf.len());
            }
        }
    }
    let at = |i: usize| &buf[if i == 0 { 0 } else { ends[i - 1] }..ends[i]];
    let mut order: Vec<usize> = (0..ends.len()).collect();
    order.sort_unstable_by(|&a, &b| at(a).cmp(at(b)));
    let (n, q) = (order.len(), points.max(1));
    (0..q.min(n))
        .filter_map(|i| {
            let (lo, hi) = (i * n / q.min(n), (i + 1) * n / q.min(n));
            (hi > lo).then(|| (at(order[hi - 1]).to_vec(), (hi - lo) as u64))
        })
        .collect()
}

/// Append `points` in wire form.
pub(crate) fn put_points(out: &mut Vec<u8>, points: &[(Vec<u8>, u64)]) {
    out.extend_from_slice(&(points.len() as u32).to_le_bytes());
    for (v, w) in points {
        out.extend_from_slice(&w.to_le_bytes());
        out.extend_from_slice(&(v.len() as u32).to_le_bytes());
        out.extend_from_slice(v);
    }
}

/// Read points in wire form at `*pos`, moving it past them; `None` for a
/// cut or malformed run.
pub(crate) fn read_points(c: &[u8], pos: &mut usize) -> Option<Vec<(Vec<u8>, u64)>> {
    let mut take = |n: usize| -> Option<&[u8]> {
        let s = c.get(*pos..pos.checked_add(n)?)?;
        *pos += n;
        Some(s)
    };
    let n = u32::from_le_bytes(take(4)?.try_into().ok()?);
    (0..n)
        .map(|_| {
            let w = u64::from_le_bytes(take(8)?.try_into().ok()?);
            let len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            Some((take(len)?.to_vec(), w))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_read_back_as_written_and_a_cut_run_is_refused() {
        let pts = vec![(b"a".to_vec(), 3), (Vec::new(), 1), (b"zz".to_vec(), 40)];
        let mut c = vec![9];
        put_points(&mut c, &pts);
        let mut pos = 1;
        assert_eq!(read_points(&c, &mut pos), Some(pts));
        assert_eq!(pos, c.len());
        let mut pos = 1;
        assert_eq!(read_points(&c[..c.len() - 1], &mut pos), None);
    }

    #[test]
    fn a_run_cut_at_any_byte_is_refused() {
        let mut c = Vec::new();
        put_points(&mut c, &[(b"ab".to_vec(), 3), (b"c".to_vec(), 1)]);
        for cut in 0..c.len() {
            assert_eq!(read_points(&c[..cut], &mut 0), None, "cut at {cut}");
        }
    }
}

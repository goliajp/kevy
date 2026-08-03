//! Impact-bucketed posting lists for [`crate::segment::TextSegment`]
//! (split out of `segment.rs` to keep it under the 500-LOC project
//! ceiling; behaviour unchanged).

use std::collections::HashMap;

/// Where one posting lives inside its list: which tf bucket, which
/// dl band, which slot of the band's ids Vec. Kept in a LIST-level
/// map so probe and swap-remove are O(1) without per-band maps.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Loc {
    tf: u32,
    band: u8,
    slot: u32,
}

/// dl → log2 band (band 0 = dl 1, band b covers 2^b..2^(b+1)-1,
/// capped at 15). The band's LOWER edge is the conservative dl for
/// bound tests: real scores inside the band can only be lower.
fn band_of(dl: u32) -> u8 {
    dl.max(1).ilog2().min(15) as u8
}

pub(crate) const BAND_MIN_DL: [u32; 16] = [
    1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768,
];

/// One tf bucket: SPARSE dl bands (band asc), each a dense id Vec —
/// ordered walks are sequential 4B scans; per-id EXACT dl comes from
/// the segment's id_dl table (bands only set the CUT granularity).
type Bands = Vec<(u8, Vec<u32>)>;

/// One token's postings, IMPACT-BUCKETED two ways: keys
/// grouped by tf (buckets tf-DESCENDING), and WITHIN a bucket ordered
/// by dl ascending. BM25 is monotone ↑tf / ↓dl, so a single-list walk
/// goes bucket-by-bucket high-tf-first and INSIDE each bucket stops
/// the moment score(tf, dl) can't reach the kth floor — exact top-K
/// visits ~k·buckets postings instead of the whole list (the
/// single-common-term shape measured 1.5ms at 66k postings under the
/// tf-only stop; dl-ordering makes the within-bucket cut).
/// Zipf text is mostly hapax legomena — a
/// unique id / email / doc number appears in exactly one row — so
/// the one-posting case stays INLINE (zero heap): the full structure
/// only materializes from the second posting on. (textgate caught
/// the eager shape at +2GiB RSS over 1M singleton tokens.)
#[derive(Debug)]
pub enum Buckets {
    One { id: u32, tf: u32, dl: u32 },
    Many(Box<ManyBuckets>),
}

#[derive(Debug, Default)]
pub struct ManyBuckets {
    /// (tf, bands) — sorted tf-descending; ≤ max_tf entries.
    buckets: Vec<(u32, Bands)>,
    /// id → location, for O(1) probe / remove across the whole list.
    pub(crate) index: HashMap<u32, Loc>,
}

impl Default for Buckets {
    fn default() -> Self {
        Buckets::Many(Box::default())
    }
}

impl ManyBuckets {
    fn insert(&mut self, tf: u32, dl: u32, id: u32) {
        let pos = self.buckets.iter().position(|(t, _)| *t <= tf);
        let slot = match pos {
            Some(i) if self.buckets[i].0 == tf => &mut self.buckets[i].1,
            Some(i) => {
                self.buckets.insert(i, (tf, Bands::new()));
                &mut self.buckets[i].1
            }
            None => {
                self.buckets.push((tf, Bands::new()));
                &mut self.buckets.last_mut().expect("just pushed").1
            }
        };
        let band = band_of(dl);
        let bi = match slot.binary_search_by_key(&band, |(b, _)| *b) {
            Ok(i) => i,
            Err(i) => {
                slot.insert(i, (band, Vec::new()));
                i
            }
        };
        let v = &mut slot[bi].1;
        self.index.insert(id, Loc { tf, band, slot: v.len() as u32 });
        v.push(id);
    }

    fn remove(&mut self, id: u32) {
        let Some(loc) = self.index.remove(&id) else { return };
        if let Some(i) = self.buckets.iter().position(|(t, _)| *t == loc.tf)
            && let Ok(bi) = self.buckets[i].1.binary_search_by_key(&loc.band, |(b, _)| *b)
        {
            let v = &mut self.buckets[i].1[bi].1;
            let si = loc.slot as usize;
            v.swap_remove(si);
            if let Some(&moved) = v.get(si) {
                self.index.get_mut(&moved).expect("indexed posting").slot = loc.slot;
            }
            if v.is_empty() {
                self.buckets[i].1.remove(bi);
            }
            if self.buckets[i].1.is_empty() {
                self.buckets.remove(i);
            }
        }
    }
}

impl Buckets {
    pub(crate) fn new_one(tf: u32, dl: u32, id: u32) -> Self {
        Buckets::One { id, tf, dl }
    }

    pub(crate) fn insert(&mut self, tf: u32, dl: u32, id: u32) {
        match self {
            Buckets::One { id: id0, tf: tf0, dl: dl0 } => {
                let (id0, tf0, dl0) = (*id0, *tf0, *dl0);
                let mut many = ManyBuckets::default();
                many.insert(tf0, dl0, id0);
                many.insert(tf, dl, id);
                *self = Buckets::Many(Box::new(many));
            }
            Buckets::Many(m) => m.insert(tf, dl, id),
        }
    }

    pub(crate) fn remove(&mut self, _tf: u32, _dl: u32, id: u32) {
        match self {
            Buckets::One { id: id0, .. } => {
                if *id0 == id {
                    // caller drops empty lists via is_empty()
                    *self = Buckets::Many(Box::default());
                }
            }
            Buckets::Many(m) => m.remove(id),
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Buckets::One { .. } => 1,
            Buckets::Many(m) => m.index.len(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The `Many` index-map entry count (`One` keeps none) — the
    /// per-posting structure term of the memory formula, exposed so
    /// the segment can maintain it by delta.
    pub(crate) fn index_len(&self) -> u64 {
        match self {
            Buckets::One { .. } => 0,
            Buckets::Many(m) => m.index.len() as u64,
        }
    }

    /// O(1): buckets are tf-descending.
    pub(crate) fn max_tf(&self) -> u32 {
        match self {
            Buckets::One { tf, .. } => *tf,
            Buckets::Many(m) => m.buckets.first().map_or(1, |(t, _)| *t),
        }
    }

    /// O(1) membership probe over the whole list.
    pub(crate) fn get(&self, id: u32) -> Option<u32> {
        match self {
            Buckets::One { id: id0, tf, .. } => (*id0 == id).then_some(*tf),
            Buckets::Many(m) => m.index.get(&id).map(|l| l.tf),
        }
    }

    /// Per-query view: tf groups descending, each with its bands
    /// ascending. The One case borrows its single posting as a
    /// 1-slice; only the tiny outer Vec allocates (per query, per
    /// list).
    pub(crate) fn tf_groups(&self) -> Vec<(u32, BandsView<'_>)> {
        match self {
            Buckets::One { id, tf, dl } => {
                vec![(*tf, BandsView::One(band_of(*dl), std::slice::from_ref(id)))]
            }
            Buckets::Many(m) => m
                .buckets
                .iter()
                .map(|(t, bands)| (*t, BandsView::Slice(bands.as_slice())))
                .collect(),
        }
    }
}

/// Bands of one tf group, borrowed — see [`Buckets::tf_groups`].
pub(crate) enum BandsView<'a> {
    One(u8, &'a [u32]),
    Slice(&'a [(u8, Vec<u32>)]),
}

impl BandsView<'_> {
    /// (band, ids) pairs, band ascending.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (u8, &[u32])> + '_ {
        let (one, slice) = match self {
            BandsView::One(b, ids) => (Some((*b, *ids)), [].as_slice()),
            BandsView::Slice(s) => (None, *s),
        };
        one.into_iter().chain(slice.iter().map(|(b, v)| (*b, v.as_slice())))
    }
}

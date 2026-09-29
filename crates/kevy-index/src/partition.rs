//! Global indexes: one index spread over the shards by value order, so a
//! read of one value range goes to the shards holding that range instead
//! of to every shard.
//!
//! An index is local by default: every shard holds the entries of the rows
//! it owns. A global index is cut into `P` partitions at `P - 1` split
//! points, byte strings in the index's own order encoding; partition `p`
//! holds the entries whose encoded value lies in `[splits[p-1],
//! splits[p])`, and lives on the shard [`partition_owner`] names. Both are
//! pure functions of the catalog, so any shard can place any value.

use crate::catalog::{Catalog, IndexKind, IndexSpec};

/// How an index is spread over the shards.
///
/// ```
/// use kevy_index::Partitioning;
/// assert_eq!(Partitioning::default(), Partitioning::Local);
/// let p = Partitioning::Global { splits: vec![b"m".to_vec()] };
/// assert_eq!((p.partition_of(b"a"), p.partition_of(b"z")), (0, 1));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Partitioning {
    /// Every shard holds the entries of the rows it owns.
    #[default]
    Local,
    /// The entries are split by value order at these points, strictly
    /// increasing byte strings of the index's order encoding.
    Global {
        /// The `P - 1` split points.
        splits: Vec<Vec<u8>>,
    },
}

static LOCAL: Partitioning = Partitioning::Local;

impl Partitioning {
    /// Whether the index is split by value.
    ///
    /// ```
    /// use kevy_index::Partitioning;
    /// assert!(Partitioning::Global { splits: vec![] }.is_global());
    /// assert!(!Partitioning::Local.is_global());
    /// ```
    pub fn is_global(&self) -> bool {
        matches!(self, Partitioning::Global { .. })
    }

    /// How many partitions there are: 1 for a local index.
    ///
    /// ```
    /// use kevy_index::Partitioning;
    /// assert_eq!(Partitioning::Local.partitions(), 1);
    /// assert_eq!(Partitioning::Global { splits: vec![b"m".to_vec()] }.partitions(), 2);
    /// ```
    pub fn partitions(&self) -> usize {
        match self {
            Partitioning::Local => 1,
            Partitioning::Global { splits } => splits.len() + 1,
        }
    }

    /// The partition an encoded value belongs to (0 for a local index).
    ///
    /// ```
    /// use kevy_index::Partitioning;
    /// let p = Partitioning::Global { splits: vec![b"g".to_vec(), b"p".to_vec()] };
    /// assert_eq!(p.partition_of(b"apple"), 0);
    /// assert_eq!(p.partition_of(b"g"), 1);
    /// assert_eq!(p.partition_of(b"zebra"), 2);
    /// ```
    pub fn partition_of(&self, enc: &[u8]) -> usize {
        match self {
            Partitioning::Local => 0,
            Partitioning::Global { splits } => splits.partition_point(|s| s.as_slice() <= enc),
        }
    }

    /// The split points as `SPLIT` values of type `ty` — the inverse of the
    /// order encoding they are stored in.
    ///
    /// ```
    /// use kevy_index::{Partitioning, ValType, order_key};
    /// let p = Partitioning::Global { splits: vec![order_key(ValType::I64, b"-40").unwrap()] };
    /// assert_eq!(p.split_values(ValType::I64), [b"-40".to_vec()]);
    /// ```
    pub fn split_values(&self, ty: crate::ValType) -> Vec<Vec<u8>> {
        let Partitioning::Global { splits } = self else { return Vec::new() };
        splits.iter().map(|s| decode_order_key(ty, s)).collect()
    }
}

/// Split points that cut the rows `points` stand for into `parts`
/// partitions of about equal size. A point is a value and the number of
/// rows it stands for; the points need not be distinct or in order. Each
/// split is the value whose rows-before count comes closest to its share,
/// strictly increasing and never the smallest value (which would leave the
/// first partition empty). Fewer come back when the rows have too few
/// distinct values — every entry of one value lives in one partition, so a
/// value holding more than its share cannot be split — and none for no
/// rows.
///
/// ```
/// use kevy_index::splits_from_weighted;
///
/// let points: Vec<(Vec<u8>, u64)> = (0..100u8).map(|v| (vec![v], 1)).collect();
/// assert_eq!(splits_from_weighted(points, 4), [vec![25], vec![50], vec![75]]);
/// // one value with most of the rows stays whole
/// let heavy = vec![(vec![1], 10), (vec![7], 80), (vec![9], 10)];
/// assert_eq!(splits_from_weighted(heavy, 4), [vec![7], vec![9]]);
/// assert!(splits_from_weighted(vec![(vec![7], 50)], 4).is_empty());
/// ```
pub fn splits_from_weighted(mut points: Vec<(Vec<u8>, u64)>, parts: usize) -> Vec<Vec<u8>> {
    points.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut distinct: Vec<(Vec<u8>, u64)> = Vec::with_capacity(points.len());
    for (v, w) in points {
        match distinct.last_mut() {
            Some((last, lw)) if *last == v => *lw += w,
            _ => distinct.push((v, w)),
        }
    }
    // before[j]: the rows whose value is below distinct[j]
    let mut before = Vec::with_capacity(distinct.len() + 1);
    let mut acc = 0u128;
    for (_, w) in &distinct {
        before.push(acc);
        acc += u128::from(*w);
    }
    before.push(acc);
    let parts = parts.max(1) as u128;
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(parts as usize - 1);
    let mut j = 1;
    for k in 1..parts {
        let target = acc * k / parts;
        while j + 1 < distinct.len() && before[j + 1] <= target {
            j += 1;
        }
        // distinct[j] starts at or below the target; its successor may be
        // the closer cut
        let pick = if j + 1 < distinct.len()
            && before[j + 1] - target < target.saturating_sub(before[j])
        {
            j + 1
        } else {
            j
        };
        let Some((v, _)) = distinct.get(pick) else { break };
        if out.last() < Some(v) {
            out.push(v.clone());
        }
    }
    out
}

/// The split point `raw`, as written after `SPLIT` or `SPLIT AT`, in
/// `spec`'s order encoding: a value of the index's type, or — for a
/// composite (`ORDERPATH`) index, whose points are frames of several
/// columns — `0x` and the encoded bytes in hex. `None` when it does not
/// read as either.
///
/// ```
/// use kevy_index::{IndexKind, IndexSpec, ValType, parse_split_point, split_point_text};
///
/// let s = IndexSpec::single_field(
///     b"age".to_vec(), b"u:".to_vec(), b"age".to_vec(), ValType::I64, IndexKind::Range,
/// );
/// let enc = parse_split_point(&s, b"40").unwrap();
/// assert_eq!(split_point_text(&s, &enc), b"40");
/// assert_eq!(parse_split_point(&s, b"forty"), None);
/// ```
pub fn parse_split_point(spec: &IndexSpec, raw: &[u8]) -> Option<Vec<u8>> {
    if spec.composite.is_none() {
        return crate::order_key(spec.ty, raw);
    }
    let hex = raw.strip_prefix(b"0x")?;
    if hex.len() % 2 != 0 {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    hex.chunks(2).map(|p| Some(digit(p[0])? << 4 | digit(p[1])?)).collect()
}

/// A split point as [`parse_split_point`] reads it back.
///
/// ```
/// use kevy_index::{IndexKind, IndexSpec, ValType, order_key, split_point_text};
///
/// let s = IndexSpec::single_field(
///     b"t".to_vec(), b"u:".to_vec(), b"t".to_vec(), ValType::F64, IndexKind::Range,
/// );
/// assert_eq!(split_point_text(&s, &order_key(ValType::F64, b"2.5").unwrap()), b"2.5");
/// ```
pub fn split_point_text(spec: &IndexSpec, enc: &[u8]) -> Vec<u8> {
    if spec.composite.is_none() {
        return decode_order_key(spec.ty, enc);
    }
    let mut out = b"0x".to_vec();
    for b in enc {
        out.extend_from_slice(format!("{b:02x}").as_bytes());
    }
    out
}

/// The text form of one order-encoded value of type `ty`.
fn decode_order_key(ty: crate::ValType, enc: &[u8]) -> Vec<u8> {
    let word = || u64::from_be_bytes(enc.try_into().unwrap_or([0; 8]));
    match ty {
        crate::ValType::I64 => (((word() ^ (1 << 63)) as i64).to_string()).into_bytes(),
        crate::ValType::F64 => {
            let m = word();
            let bits = if m >> 63 == 1 { m & !(1 << 63) } else { !m };
            f64::from_bits(bits).to_string().into_bytes()
        }
        _ => enc.to_vec(),
    }
}

/// The shard partition `p` of the index `name` lives on, of `n` shards.
/// The name shifts the start, so the first partitions of different indexes
/// do not all land on shard 0.
///
/// ```
/// let n = 4;
/// let owners: Vec<usize> = (0..n).map(|p| kevy_index::partition_owner(b"by_age", p, n)).collect();
/// let mut sorted = owners.clone();
/// sorted.sort();
/// assert_eq!(sorted, [0, 1, 2, 3], "one partition per shard");
/// ```
pub fn partition_owner(name: &[u8], p: usize, n: usize) -> usize {
    // FNV-1a: stable across builds and versions, which a placement must be
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in name {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    (p + (h % n as u64) as usize) % n
}

/// Why a global partitioning cannot go on this index.
fn global_guard(spec: &IndexSpec, splits: &[Vec<u8>]) -> Result<(), &'static str> {
    if !matches!(spec.kind, IndexKind::Range | IndexKind::Unique) {
        return Err("ERR PARTITION global requires KIND range|unique");
    }
    if splits.windows(2).any(|w| w[0] >= w[1]) {
        return Err("ERR SPLIT AT values must be strictly increasing");
    }
    Ok(())
}

impl Catalog {
    /// [`Catalog::create`] with a partitioning. A global one is refused by
    /// name where it cannot apply: kinds whose segments are not a
    /// `(value, key)` order, and split points out of order.
    ///
    /// ```
    /// use kevy_index::{Catalog, IndexKind, IndexSpec, Partitioning, ValType, order_key};
    ///
    /// let spec = IndexSpec::single_field(
    ///     b"age".to_vec(), b"user:".to_vec(), b"age".to_vec(), ValType::I64, IndexKind::Range,
    /// );
    /// let mut c = Catalog::new();
    /// let split = order_key(ValType::I64, b"40").unwrap();
    /// c.create_with(spec, Partitioning::Global { splits: vec![split] }).unwrap();
    /// assert_eq!(c.partitioning(b"age").partitions(), 2);
    /// assert!(!c.partitioning(b"other").is_global());
    /// ```
    pub fn create_with(
        &mut self,
        spec: IndexSpec,
        partitioning: Partitioning,
    ) -> Result<(), &'static str> {
        if let Partitioning::Global { splits } = &partitioning {
            global_guard(&spec, splits)?;
        }
        let name = spec.name.clone();
        self.create(spec)?;
        if partitioning.is_global() {
            self.parts.push((name, partitioning));
        }
        Ok(())
    }

    /// How the index `name` is spread (local when it is not global, or
    /// not there).
    pub fn partitioning(&self, name: &[u8]) -> &Partitioning {
        self.parts.iter().find(|(n, _)| n == name).map_or(&LOCAL, |(_, p)| p)
    }

    /// Re-split a global index (a rebuild re-sampled it). `false` when
    /// `name` is not a global index of this catalog, or `splits` are out
    /// of order.
    ///
    /// ```
    /// use kevy_index::{Catalog, IndexKind, IndexSpec, Partitioning, ValType};
    ///
    /// let spec = IndexSpec::single_field(
    ///     b"name".to_vec(), b"user:".to_vec(), b"name".to_vec(), ValType::Str, IndexKind::Range,
    /// );
    /// let mut c = Catalog::new();
    /// c.create_with(spec, Partitioning::Global { splits: vec![] }).unwrap();
    /// assert!(c.set_splits(b"name", vec![b"h".to_vec(), b"q".to_vec()]));
    /// assert_eq!(c.partitioning(b"name").partition_of(b"mia"), 1);
    /// ```
    pub fn set_splits(&mut self, name: &[u8], splits: Vec<Vec<u8>>) -> bool {
        if splits.windows(2).any(|w| w[0] >= w[1]) {
            return false;
        }
        match self.parts.iter_mut().find(|(n, _)| n == name) {
            Some((_, p)) => {
                *p = Partitioning::Global { splits };
                true
            }
            None => false,
        }
    }
}

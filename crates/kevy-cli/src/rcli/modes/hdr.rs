//! A high-dynamic-range histogram of byte sizes: exact below 32768, and
//! within 0.01% above, up to 1 TB.
//!
//! The bucket layout and the percentile walk follow HdrHistogram (public
//! domain), because the report's rows are defined by them. Storage is ours:
//! only the buckets a value landed in exist, where the reference allocates
//! all 458,752 counters up front for four significant figures.

use std::collections::BTreeMap;

/// Values up to `2 * 10^4` resolve exactly: sub-buckets of 2^15.
const SUB_BUCKET_HALF_MAGNITUDE: u32 = 14;
const SUB_BUCKET_HALF: u64 = 1 << SUB_BUCKET_HALF_MAGNITUDE;
const SUB_BUCKET_MASK: u64 = (1 << (SUB_BUCKET_HALF_MAGNITUDE + 1)) - 1;
/// The largest value recorded; larger ones are not.
const HIGHEST: u64 = 1 << 40;

#[derive(Debug, Default)]
pub(crate) struct Histogram {
    /// Counts by bucket position, which orders values.
    counts: BTreeMap<u64, u64>,
    total: u64,
}

/// One row of the percentile walk.
#[derive(Debug, PartialEq)]
pub(crate) struct Row {
    /// The highest value equivalent to the bucket reached.
    pub(crate) value: u64,
    /// Values at or below it.
    pub(crate) cumulative: u64,
}

impl Histogram {
    pub(crate) fn record(&mut self, value: u64) {
        if value > HIGHEST {
            return;
        }
        *self.counts.entry(index_of(value)).or_default() += 1;
        self.total += 1;
    }

    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    /// Rows at percentiles 0, 50, 75, 87.5, … (each step half the distance
    /// left to 100), each bucket reported once.
    pub(crate) fn percentile_rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = Vec::new();
        let mut target = 0.0f64;
        let mut cumulative = 0u64;
        for (&index, &count) in &self.counts {
            cumulative += count;
            let reached = 100.0 * cumulative as f64 / self.total as f64;
            if target > reached {
                continue;
            }
            let value = highest_equivalent(value_at(index));
            if rows.last().is_none_or(|r| r.value != value) {
                rows.push(Row { value, cumulative });
            }
            // The last bucket is reported once; before it, the targets this
            // bucket already covers are passed over.
            while cumulative < self.total && target <= reached {
                target = next_target(target);
            }
        }
        rows
    }

    /// The mean and standard deviation, each value taken at the middle of
    /// its bucket.
    pub(crate) fn mean_and_deviation(&self) -> (f64, f64) {
        if self.total == 0 {
            return (0.0, 0.0);
        }
        let middle = |index: u64| median_equivalent(value_at(index)) as f64;
        let sum: f64 = self.counts.iter().map(|(&i, &c)| c as f64 * middle(i)).sum();
        let mean = sum / self.total as f64;
        let spread: f64 =
            self.counts.iter().map(|(&i, &c)| c as f64 * (middle(i) - mean).powi(2)).sum();
        (mean, (spread / self.total as f64).sqrt())
    }
}

/// The next percentile to report after `p`: ticks double each time the
/// distance to 100 halves.
fn next_target(p: f64) -> f64 {
    let halvings = (100.0 / (100.0 - p)).log2() as i64 + 1;
    let ticks = 2f64.powi(halvings as i32);
    p + 100.0 / ticks
}

fn bucket_of(value: u64) -> (u32, u64) {
    let pow2ceiling = 64 - (value | SUB_BUCKET_MASK).leading_zeros();
    let bucket = pow2ceiling - (SUB_BUCKET_HALF_MAGNITUDE + 1);
    (bucket, value >> bucket)
}

/// Bucket and sub-bucket as one ordered position: the top half of each
/// bucket's sub-buckets follows the bucket before it.
fn index_of(value: u64) -> u64 {
    let (bucket, sub) = bucket_of(value);
    (u64::from(bucket) << SUB_BUCKET_HALF_MAGNITUDE) + sub
}

/// The lowest value of the bucket at `index`.
fn value_at(index: u64) -> u64 {
    let mut bucket = (index >> SUB_BUCKET_HALF_MAGNITUDE) as i64 - 1;
    let mut sub = (index & (SUB_BUCKET_HALF - 1)) + SUB_BUCKET_HALF;
    if bucket < 0 {
        sub -= SUB_BUCKET_HALF;
        bucket = 0;
    }
    sub << bucket
}

fn range_size(value: u64) -> u64 {
    let (bucket, sub) = bucket_of(value);
    let adjusted = if sub >= 2 * SUB_BUCKET_HALF { bucket + 1 } else { bucket };
    1 << adjusted
}

fn lowest_equivalent(value: u64) -> u64 {
    let (bucket, sub) = bucket_of(value);
    sub << bucket
}

fn highest_equivalent(value: u64) -> u64 {
    lowest_equivalent(value) + range_size(value) - 1
}

fn median_equivalent(value: u64) -> u64 {
    lowest_equivalent(value) + (range_size(value) >> 1)
}

#[cfg(test)]
mod tests {
    use super::{Histogram, Row, highest_equivalent, median_equivalent};

    fn of(values: &[u64]) -> Histogram {
        let mut h = Histogram::default();
        values.iter().for_each(|&v| h.record(v));
        h
    }

    #[test]
    fn small_values_are_exact_and_rows_follow_halving_percentiles() {
        let h = of(&[32, 37, 37, 39, 56]);
        let rows = h.percentile_rows();
        let expect = [(32, 1), (37, 3), (39, 4), (56, 5)]
            .map(|(value, cumulative)| Row { value, cumulative });
        assert_eq!(rows, expect);
        let rows = of(&[30, 40, 80, 281, 334, 81944, 81944, 81944]).percentile_rows();
        assert_eq!(rows.iter().map(|r| r.cumulative).collect::<Vec<_>>(), [1, 4, 8]);
        assert_eq!(rows[0].value, 30);
        assert_eq!(rows[1].value, 281);
    }

    #[test]
    fn large_values_share_buckets() {
        assert_eq!(highest_equivalent(1000), 1000);
        assert_eq!(highest_equivalent(80020), 80023);
        assert_eq!(median_equivalent(80020), 80022);
        let (mean, dev) = of(&[10, 20]).mean_and_deviation();
        assert_eq!((mean, dev), (15.0, 5.0));
        assert_eq!(Histogram::default().mean_and_deviation(), (0.0, 0.0));
        let mut h = Histogram::default();
        h.record((1 << 40) + 1);
        assert_eq!(h.total(), 0, "beyond 1 TB is not recorded");
    }
}

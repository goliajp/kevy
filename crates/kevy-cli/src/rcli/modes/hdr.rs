//! A high-dynamic-range histogram: exact up to a few thousand, then within a
//! fixed relative error, up to a highest trackable value.
//!
//! The bucket layout, the percentile walk and value-at-percentile follow
//! HdrHistogram (public domain), because the reports print exactly those
//! values. Storage is ours: only the buckets a value landed in exist, where
//! the reference allocates every counter up front (458,752 of them for four
//! significant figures up to 1 TB).

use std::collections::BTreeMap;

/// The bucket geometry for a number of significant figures.
#[derive(Clone, Copy, Debug)]
struct Layout {
    /// log2 of half the sub-buckets per bucket.
    half_magnitude: u32,
    /// The largest value recorded; larger ones are not.
    highest: u64,
}

#[derive(Debug)]
pub(crate) struct Histogram {
    layout: Layout,
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
    /// Byte sizes: four significant figures, up to 1 TB.
    pub(crate) fn sizes() -> Histogram {
        Histogram::new(4, 1 << 40)
    }

    /// Microseconds: three significant figures, up to 60 s.
    pub(crate) fn latencies() -> Histogram {
        Histogram::new(3, 60_000_000)
    }

    fn new(figures: u32, highest: u64) -> Histogram {
        // Single-unit resolution up to 2 * 10^figures, in sub-buckets of the
        // next power of two.
        let single = 2 * 10u64.pow(figures);
        let magnitude = 64 - (single - 1).leading_zeros();
        Histogram {
            layout: Layout { half_magnitude: magnitude - 1, highest },
            counts: BTreeMap::new(),
            total: 0,
        }
    }

    pub(crate) fn record(&mut self, value: u64) {
        if value > self.layout.highest {
            return;
        }
        *self.counts.entry(self.layout.index_of(value)).or_default() += 1;
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
            let value = self.layout.highest_equivalent(self.layout.value_at(index));
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

    /// The value below which `percentile` percent of values fall: the
    /// highest value of the bucket holding that rank. 0 when empty.
    pub(crate) fn value_at_percentile(&self, percentile: f64) -> u64 {
        let wanted = ((percentile.min(100.0) / 100.0 * self.total as f64 + 0.5) as u64).max(1);
        let mut cumulative = 0;
        for (&index, &count) in &self.counts {
            cumulative += count;
            if cumulative >= wanted {
                return self.layout.highest_equivalent(self.layout.value_at(index));
            }
        }
        0
    }

    /// The mean and standard deviation, each value taken at the middle of
    /// its bucket.
    pub(crate) fn mean_and_deviation(&self) -> (f64, f64) {
        if self.total == 0 {
            return (0.0, 0.0);
        }
        let middle = |index: u64| self.layout.median_equivalent(self.layout.value_at(index)) as f64;
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

impl Layout {
    fn half(self) -> u64 {
        1 << self.half_magnitude
    }

    fn bucket_of(self, value: u64) -> (u32, u64) {
        let mask = (1u64 << (self.half_magnitude + 1)) - 1;
        let pow2ceiling = 64 - (value | mask).leading_zeros();
        let bucket = pow2ceiling - (self.half_magnitude + 1);
        (bucket, value >> bucket)
    }

    /// Bucket and sub-bucket as one ordered position: the top half of each
    /// bucket's sub-buckets follows the bucket before it.
    fn index_of(self, value: u64) -> u64 {
        let (bucket, sub) = self.bucket_of(value);
        (u64::from(bucket) << self.half_magnitude) + sub
    }

    /// The lowest value of the bucket at `index`.
    fn value_at(self, index: u64) -> u64 {
        let bucket = (index >> self.half_magnitude) as i64 - 1;
        let sub = (index & (self.half() - 1)) + self.half();
        if bucket < 0 { sub - self.half() } else { sub << bucket }
    }

    fn range_size(self, value: u64) -> u64 {
        let (bucket, sub) = self.bucket_of(value);
        let adjusted = if sub >= 2 * self.half() { bucket + 1 } else { bucket };
        1 << adjusted
    }

    fn lowest_equivalent(self, value: u64) -> u64 {
        let (bucket, sub) = self.bucket_of(value);
        sub << bucket
    }

    fn highest_equivalent(self, value: u64) -> u64 {
        self.lowest_equivalent(value) + self.range_size(value) - 1
    }

    fn median_equivalent(self, value: u64) -> u64 {
        self.lowest_equivalent(value) + (self.range_size(value) >> 1)
    }
}

#[cfg(test)]
mod tests {
    use super::{Histogram, Row};

    fn of(make: fn() -> Histogram, values: &[u64]) -> Histogram {
        let mut h = make();
        values.iter().for_each(|&v| h.record(v));
        h
    }

    #[test]
    fn small_values_are_exact_and_rows_follow_halving_percentiles() {
        let rows = of(Histogram::sizes, &[32, 37, 37, 39, 56]).percentile_rows();
        let expect = [(32, 1), (37, 3), (39, 4), (56, 5)]
            .map(|(value, cumulative)| Row { value, cumulative });
        assert_eq!(rows, expect);
        let rows =
            of(Histogram::sizes, &[30, 40, 80, 281, 334, 81944, 81944, 81944]).percentile_rows();
        assert_eq!(rows.iter().map(|r| r.cumulative).collect::<Vec<_>>(), [1, 4, 8]);
        assert_eq!((rows[0].value, rows[1].value), (30, 281));
    }

    #[test]
    fn large_values_share_buckets() {
        let sizes = Histogram::sizes().layout;
        assert_eq!(sizes.highest_equivalent(1000), 1000);
        assert_eq!(sizes.highest_equivalent(80020), 80023);
        assert_eq!(sizes.median_equivalent(80020), 80022);
        let latencies = Histogram::latencies().layout;
        assert_eq!(latencies.highest_equivalent(2047), 2047, "three figures: exact below 2048");
        assert_eq!(latencies.highest_equivalent(5000), 5003);
        let (mean, dev) = of(Histogram::sizes, &[10, 20]).mean_and_deviation();
        assert_eq!((mean, dev), (15.0, 5.0));
        assert_eq!(Histogram::sizes().mean_and_deviation(), (0.0, 0.0));
        let mut h = Histogram::latencies();
        h.record(60_000_001);
        assert_eq!(h.total(), 0, "past the highest trackable value");
    }

    #[test]
    fn value_at_percentile_is_the_bucket_holding_that_rank() {
        let h = of(Histogram::latencies, &[100, 200, 300, 400]);
        assert_eq!(h.value_at_percentile(50.0), 200);
        assert_eq!(h.value_at_percentile(99.0), 400);
        assert_eq!(h.value_at_percentile(0.0), 100, "at least the first");
        assert_eq!(h.value_at_percentile(150.0), 400);
        assert_eq!(Histogram::latencies().value_at_percentile(50.0), 0);
    }
}

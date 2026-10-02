//! The cardinality estimate of Ertl, "New cardinality estimation
//! algorithms for HyperLogLog sketches" (2017), over a histogram of
//! register values.

use super::{Q, REGISTERS};

/// `0.5 / ln 2`.
const ALPHA_INF: f64 = 0.721_347_520_444_481_7;

fn sigma(mut x: f64) -> f64 {
    if x == 1.0 {
        return f64::INFINITY;
    }
    let (mut y, mut z) = (1.0, x);
    loop {
        x *= x;
        let before = z;
        z += x * y;
        y += y;
        if z == before {
            return z;
        }
    }
}

// the same bits either way: both roots are IEEE's correctly rounded one
#[cfg(feature = "std")]
fn root(x: f64) -> f64 {
    x.sqrt()
}

#[cfg(not(feature = "std"))]
fn root(x: f64) -> f64 {
    kevy_num::sqrt(x)
}

fn tau(mut x: f64) -> f64 {
    if x == 0.0 || x == 1.0 {
        return 0.0;
    }
    let (mut y, mut z) = (1.0, 1.0 - x);
    loop {
        x = root(x);
        let before = z;
        y *= 0.5;
        z -= (1.0 - x) * (1.0 - x) * y;
        if z == before {
            return z / 3.0;
        }
    }
}

/// The estimate from how many registers hold each value.
pub(super) fn estimate(histogram: &[u32; 64]) -> u64 {
    let m = REGISTERS as f64;
    let mut z = m * tau((m - f64::from(histogram[Q + 1])) / m);
    for j in (1..=Q).rev() {
        z += f64::from(histogram[j]);
        z *= 0.5;
    }
    z += m * sigma(f64::from(histogram[0]) / m);
    round(ALPHA_INF * m * m / z)
}

/// To the nearest integer, halves away from zero, as `llroundl` rounds a
/// positive value. Past 2^52 every double is already an integer.
fn round(v: f64) -> u64 {
    let whole = v as u64;
    whole + u64::from(v - whole as f64 >= 0.5)
}

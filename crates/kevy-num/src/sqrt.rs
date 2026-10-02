//! The square root of a double, correctly rounded as IEEE 754 requires —
//! the bits a hardware `sqrt` gives — computed with integers, for builds
//! without `std`.

/// `√x`, correctly rounded: NaN for a negative or NaN `x`, `x` itself
/// for ±0 and +∞.
///
/// ```
/// assert_eq!(kevy_num::sqrt(2.0), 1.4142135623730951);
/// assert_eq!(kevy_num::sqrt(0.25), 0.5);
/// assert_eq!(kevy_num::sqrt(5e-324), 2.2227587494850775e-162);
/// assert!(kevy_num::sqrt(-1.0).is_nan());
/// ```
pub fn sqrt(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 || x.is_infinite() {
        return x;
    }
    let bits = x.to_bits();
    let (m, e) = match (bits >> 52) & 0x7ff {
        0 => (bits & ((1 << 52) - 1), -1074i64),
        ex => ((bits & ((1 << 52) - 1)) | (1 << 52), ex as i64 - 1075),
    };
    // x = m · 2^e, m normalized to 53 bits and e made even
    let lz = i64::from(m.leading_zeros()) - 11;
    let (mut m, mut e) = (u128::from(m) << lz, e - lz);
    if e & 1 != 0 {
        m <<= 1;
        e -= 1;
    }
    // √(m · 2^58) has 55 or 56 bits: two or three past the 53 kept
    const K: i64 = 29;
    let n = m << (2 * K);
    let q = isqrt(n);
    let sticky = q * q != n;
    let drop = (128 - q.leading_zeros()) as i64 - 53;
    let low = q & ((1 << drop) - 1);
    let half = 1u128 << (drop - 1);
    let mut kept = q >> drop;
    if low > half || (low == half && (sticky || kept & 1 == 1)) {
        kept += 1;
    }
    let mut exp = e / 2 - K + drop;
    if kept >> 53 != 0 {
        kept >>= 1;
        exp += 1;
    }
    // a root's exponent is half its square's: always a normal double
    f64::from_bits((((exp + 52 + 1023) as u64) << 52) | (kept as u64 & ((1 << 52) - 1)))
}

/// `⌊√n⌋` by Newton's iteration from above.
fn isqrt(n: u128) -> u128 {
    let mut x = 1u128 << (128 - n.leading_zeros()).div_ceil(2);
    loop {
        let y = (x + n / x) / 2;
        if y >= x {
            return x;
        }
        x = y;
    }
}

#[cfg(test)]
mod tests {
    // the hardware root is the reference: std's sqrt is IEEE's
    extern crate std;

    #[test]
    fn matches_the_hardware_root() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut checked = 0;
        for _ in 0..2_000_000 {
            let x = f64::from_bits(next() & !(1 << 63));
            if x.is_nan() {
                continue;
            }
            assert_eq!(super::sqrt(x).to_bits(), x.sqrt().to_bits(), "{x:e}");
            checked += 1;
        }
        for x in [f64::MIN_POSITIVE, 5e-324, f64::MAX, 1.0, 4.0, 2.0, 1e-300, 0.5] {
            assert_eq!(super::sqrt(x).to_bits(), x.sqrt().to_bits(), "{x:e}");
        }
        assert!(checked > 1_900_000);
    }
}

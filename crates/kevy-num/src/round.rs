//! A literal to the nearest double, ties to even, as `strtod` rounds.

use crate::scan::{Kind, Literal};

/// The double `lit` spells and whether it fell out of the double's range.
pub(crate) fn to_f64(lit: Literal<'_>) -> (f64, bool) {
    let (v, out_of_range) = match lit.kind {
        Kind::Infinity => (f64::INFINITY, false),
        Kind::NotANumber => (f64::NAN, false),
        Kind::Decimal { digits, text, .. } => {
            // the scanner only lets through what std's grammar also takes
            let v: f64 = core::str::from_utf8(text)
                .ok()
                .and_then(|t| t.parse().ok())
                .expect("a scanned decimal literal");
            let nonzero = digits.iter().any(|c| (b'1'..=b'9').contains(c));
            (v, v.is_infinite() || (v == 0.0 && nonzero))
        }
        Kind::Hex { digits, exp } => hex_to_f64(digits, exp),
    };
    (if lit.negative { -v } else { v }, out_of_range)
}

/// Up to 16 significant hex digits as an integer, the power of two that
/// scales it, and whether any nonzero digit was dropped past them.
pub(crate) fn hex_significand(digits: &[u8], exp: i64) -> (u64, i64, bool) {
    let (m, e2, sticky) = hex_bits(digits, exp, 16);
    (m as u64, e2, sticky)
}

/// Up to `keep` (≤ 32) significant hex digits as an integer, the power of
/// two that scales it, and whether any nonzero digit was dropped past them.
pub(crate) fn hex_bits(digits: &[u8], exp: i64, keep: u32) -> (u128, i64, bool) {
    let (mut m, mut e2, mut kept, mut sticky, mut after_point) = (0u128, exp, 0, false, false);
    for &c in digits {
        if c == b'.' {
            after_point = true;
            continue;
        }
        let d = u128::from((c as char).to_digit(16).expect("a hex digit"));
        if kept == keep {
            sticky |= d != 0;
            if !after_point {
                e2 += 4;
            }
            continue;
        }
        if m == 0 && d == 0 {
            if after_point {
                e2 -= 4;
            }
            continue;
        }
        m = (m << 4) | d;
        kept += 1;
        if after_point {
            e2 -= 4;
        }
    }
    (m, e2, sticky)
}

/// `m × 2^e2` (plus a sliver below `m` when `sticky`) to the nearest
/// double, and whether it overflowed or underflowed to zero.
fn hex_to_f64(digits: &[u8], exp: i64) -> (f64, bool) {
    let (m, e2, sticky) = hex_significand(digits, exp);
    if m == 0 {
        return (0.0, false);
    }
    let lz = m.leading_zeros();
    let m = m << lz;
    // the value is m × 2^(top - 63), m's top bit set
    let top = e2 - i64::from(lz) + 63;
    if top > 1023 {
        return (f64::INFINITY, true);
    }
    // bits of m dropped to fit 53, more below the smallest normal
    let shift = (11 + (-1022 - top).max(0)) as u32;
    let kept = if shift >= 64 { 0 } else { m >> shift };
    let rest = if shift >= 64 { u128::from(m) } else { u128::from(m) & ((1u128 << shift) - 1) };
    let half = if shift > 64 { 0 } else { 1u128 << (shift - 1) };
    let up = shift <= 64 && (rest > half || (rest == half && (sticky || kept & 1 == 1)));
    let kept = kept + u64::from(up);
    let bits = if top < -1022 {
        // subnormal: a carry into bit 52 lands on the smallest normal
        kept
    } else if kept == 1 << 53 {
        ((top + 1 + 1023) as u64) << 52
    } else {
        (((top + 1023) as u64) << 52) | (kept & ((1 << 52) - 1))
    };
    if bits >= 0x7ff0_0000_0000_0000 {
        return (f64::INFINITY, true);
    }
    (f64::from_bits(bits), bits == 0)
}

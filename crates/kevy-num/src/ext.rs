//! The x87 80-bit extended-precision format: a 64-bit significand with an
//! explicit integer bit and a 15-bit exponent, as `long double` is on
//! x86-64. Read, added and printed exactly, rounding to nearest, ties to
//! even.

use alloc::vec::Vec;

use crate::big::Big;
use crate::scan::{Kind, scan};

/// The weight of a subnormal's lowest bit.
const MIN_EXP: i64 = -16445;
/// The highest exponent of a normal value's lowest bit: (2^64-1)·2^16320
/// is the largest finite value.
const MAX_EXP: i64 = 16320;

/// An x87 extended-precision value.
///
/// ```
/// use kevy_num::LongDouble;
/// let a = LongDouble::parse_exact(b"1.1").unwrap();
/// let b = LongDouble::parse_exact(b"2.2").unwrap();
/// let mut out = Vec::new();
/// (a + b).write_fixed(&mut out, 17);
/// assert_eq!(out, b"3.30000000000000000");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LongDouble {
    negative: bool,
    value: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Value {
    Zero,
    Infinite,
    NotANumber,
    /// `m · 2^e`: `m`'s top bit set unless `e` is [`MIN_EXP`].
    Finite {
        m: u64,
        e: i64,
    },
}

impl LongDouble {
    /// `b` as a long double when all of it is one `strtold` literal: no
    /// white space around it, not NaN, and inside the format's range.
    /// Infinities pass.
    ///
    /// ```
    /// use kevy_num::LongDouble;
    /// assert!(LongDouble::parse_exact(b"1e4932").is_some(), "past a double's range");
    /// assert!(LongDouble::parse_exact(b"1e4933").is_none());
    /// assert!(LongDouble::parse_exact(b"1e-4960").is_none(), "nonzero, read as zero");
    /// assert!(LongDouble::parse_exact(b" 1").is_none());
    /// ```
    pub fn parse_exact(b: &[u8]) -> Option<LongDouble> {
        let (lit, n) = scan(b)?;
        if n != b.len() {
            return None;
        }
        let negative = lit.negative;
        let (value, out_of_range) = match lit.kind {
            Kind::Infinity => (Value::Infinite, false),
            Kind::NotANumber => return None,
            Kind::Decimal { digits, exp, .. } => decimal(digits, exp),
            Kind::Hex { digits, exp } => hex(digits, exp),
        };
        (!out_of_range).then_some(LongDouble { negative, value })
    }

    /// Whether the value is neither infinite nor NaN.
    ///
    /// ```
    /// use kevy_num::LongDouble;
    /// assert!(LongDouble::parse_exact(b"-2").unwrap().is_finite());
    /// assert!(!LongDouble::parse_exact(b"inf").unwrap().is_finite());
    /// ```
    pub fn is_finite(&self) -> bool {
        matches!(self.value, Value::Zero | Value::Finite { .. })
    }

    /// Append the value as C's `printf("%.<decimals>Lf")` prints it.
    ///
    /// ```
    /// use kevy_num::LongDouble;
    /// let mut out = Vec::new();
    /// LongDouble::parse_exact(b"-0.5").unwrap().write_fixed(&mut out, 0);
    /// assert_eq!(out, b"-0", "half to even");
    /// ```
    pub fn write_fixed(&self, out: &mut Vec<u8>, decimals: u32) {
        if self.negative {
            out.push(b'-');
        }
        let digits = match self.value {
            Value::NotANumber => return out.extend_from_slice(b"nan"),
            Value::Infinite => return out.extend_from_slice(b"inf"),
            Value::Zero => Big::default(),
            Value::Finite { m, e } => scaled(m, e, decimals),
        };
        let mut text = digits.to_decimal();
        let d = decimals as usize;
        if text.len() <= d {
            let mut padded = alloc::vec![b'0'; d + 1 - text.len()];
            padded.extend_from_slice(&text);
            text = padded;
        }
        let point = text.len() - d;
        out.extend_from_slice(&text[..point]);
        if d > 0 {
            out.push(b'.');
            out.extend_from_slice(&text[point..]);
        }
    }
}

/// A double exactly: every double is a long double.
///
/// ```
/// use kevy_num::LongDouble;
/// let mut out = Vec::new();
/// LongDouble::from(0.1).write_fixed(&mut out, 20);
/// assert_eq!(out, b"0.10000000000000000555");
/// ```
impl From<f64> for LongDouble {
    fn from(v: f64) -> LongDouble {
        let negative = v.is_sign_negative();
        let value = if v.is_nan() {
            Value::NotANumber
        } else if v.is_infinite() {
            Value::Infinite
        } else if v == 0.0 {
            Value::Zero
        } else {
            let bits = v.to_bits();
            let (f, e) = match (bits >> 52) & 0x7ff {
                0 => (bits & ((1 << 52) - 1), -1074),
                ex => ((bits & ((1 << 52) - 1)) | (1 << 52), ex as i64 - 1075),
            };
            let lz = f.leading_zeros();
            Value::Finite { m: f << lz, e: e - i64::from(lz) }
        };
        LongDouble { negative, value }
    }
}

/// The sum, rounded once.
///
/// ```
/// use kevy_num::LongDouble;
/// let big = LongDouble::parse_exact(b"1e4932").unwrap();
/// assert!(!(big + big).is_finite(), "overflows to infinity");
/// ```
impl core::ops::Add for LongDouble {
    type Output = LongDouble;

    fn add(self, o: LongDouble) -> LongDouble {
        use Value::*;
        let value = match (self.value, o.value) {
            (NotANumber, _) | (_, NotANumber) => NotANumber,
            (Infinite, Infinite) if self.negative != o.negative => NotANumber,
            (Infinite, _) => return self,
            (_, Infinite) | (Zero, Finite { .. }) => return o,
            (Finite { .. }, Zero) => return self,
            // only -0 + -0 keeps the sign
            (Zero, Zero) => {
                return LongDouble { negative: self.negative && o.negative, value: Zero };
            }
            (Finite { m: ma, e: ea }, Finite { m: mb, e: eb }) => {
                let e = ea.min(eb);
                let (mut a, mut b) = (Big::from_u64(ma), Big::from_u64(mb));
                a.shl((ea - e) as u64);
                b.shl((eb - e) as u64);
                if self.negative == o.negative {
                    return LongDouble {
                        negative: self.negative,
                        value: round(a.add(&b), e, false),
                    };
                }
                let (d, b_larger) = a.abs_diff(&b);
                let negative = if b_larger { o.negative } else { self.negative };
                return LongDouble {
                    negative: negative && !d.is_zero(),
                    value: round(d, e, false),
                };
            }
        };
        // the x87 "indefinite" NaN carries the sign bit
        LongDouble { negative: true, value }
    }
}

/// `m · 2^e · 10^decimals`, rounded to an integer, ties to even.
fn scaled(m: u64, e: i64, decimals: u32) -> Big {
    let mut x = Big::from_u64(m);
    x.mul_pow10(u64::from(decimals));
    if e >= 0 {
        x.shl(e as u64);
        return x;
    }
    let shift = (-e) as u64;
    let half = x.bit(shift - 1);
    let below = x.any_below(shift - 1);
    x.shr(shift);
    if half && (below || x.bit(0)) {
        x.mul_add_small(1, 1);
    }
    x
}

/// `n · 2^e`, plus a sliver below `n` when `sticky`, to the format.
fn round(n: Big, e: i64, sticky: bool) -> Value {
    if n.is_zero() {
        return Value::Zero;
    }
    let top = e + n.bit_len() as i64 - 1;
    let lsb = (top - 63).max(MIN_EXP);
    let drop = lsb - e;
    let (mut kept, mut lsb) = if drop <= 0 {
        (n.bits_from(0) << (-drop), lsb)
    } else {
        let d = drop as u64;
        let (half, below) = (n.bit(d - 1), sticky || n.any_below(d - 1));
        let kept = n.bits_from(d);
        (kept + u128::from(half && (below || kept & 1 == 1)), lsb)
    };
    if kept >> 64 != 0 {
        kept >>= 1;
        lsb += 1;
    }
    if kept == 0 {
        return Value::Zero;
    }
    if lsb > MAX_EXP {
        return Value::Infinite;
    }
    Value::Finite { m: kept as u64, e: lsb }
}

/// A decimal literal's value, and whether it fell out of range.
fn decimal(digits: &[u8], exp: i64) -> (Value, bool) {
    let mut n = Big::default();
    let mut frac = 0i64;
    let mut seen_point = false;
    for &c in digits {
        if c == b'.' {
            seen_point = true;
            continue;
        }
        n.mul_add_small(10, u32::from(c - b'0'));
        frac += i64::from(seen_point);
    }
    if n.is_zero() {
        return (Value::Zero, false);
    }
    let k = exp - frac;
    // the value's order of magnitude, within one
    let magnitude = n.to_decimal().len() as i64 + k;
    if magnitude > 4934 {
        return (Value::Infinite, true);
    }
    if magnitude < -4952 {
        return (Value::Zero, true);
    }
    let v = if k >= 0 {
        n.mul_pow10(k as u64);
        round(n, 0, false)
    } else {
        let mut d = Big::from_u64(1);
        d.mul_pow10((-k) as u64);
        let s = 70 + d.bit_len() as i64 - n.bit_len() as i64;
        if s >= 0 {
            n.shl(s as u64)
        } else {
            d.shl((-s) as u64)
        }
        let (q, rest) = n.div_small_quotient(&d);
        round(big_from_u128(q), -s, rest)
    };
    (v, matches!(v, Value::Zero | Value::Infinite))
}

/// A hexadecimal literal's value, and whether it fell out of range.
fn hex(digits: &[u8], exp: i64) -> (Value, bool) {
    let (q, e2, sticky) = crate::round::hex_bits(digits, exp, 30);
    if q == 0 {
        return (Value::Zero, false);
    }
    let v = round(big_from_u128(q), e2, sticky);
    (v, matches!(v, Value::Zero | Value::Infinite))
}

fn big_from_u128(q: u128) -> Big {
    let mut b = Big::from_u64((q >> 64) as u64);
    b.shl(64);
    b.add(&Big::from_u64(q as u64))
}

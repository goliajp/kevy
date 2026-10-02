//! Reading a number the way C's `strtod` reads it: leading white space,
//! a sign, then a decimal, a hexadecimal (`0x1.8p3`), `inf` / `infinity`
//! or `nan` in any case, taking the longest prefix that is one.

/// What [`strtod`] read from the front of the input.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct Scanned {
    /// The value, correctly rounded; 0 when nothing was read.
    pub value: f64,
    /// Bytes read, the skipped white space included; 0 when nothing was.
    pub len: usize,
    /// The literal was finite but its value is not: it overflowed to an
    /// infinity, or a nonzero literal underflowed to zero.
    pub out_of_range: bool,
}

/// A number literal: its sign, and what it spells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Literal<'a> {
    pub(crate) negative: bool,
    pub(crate) kind: Kind<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind<'a> {
    Infinity,
    NotANumber,
    /// Digits with at most one `.`, and a power of ten; `text` is the
    /// whole literal without its sign, exponent included.
    Decimal {
        digits: &'a [u8],
        exp: i64,
        text: &'a [u8],
    },
    /// Hex digits with at most one `.`, and a power of two.
    Hex {
        digits: &'a [u8],
        exp: i64,
    },
}

fn is_c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `strtod`: the number at the front of `b` after any white space.
///
/// ```
/// use kevy_num::strtod;
/// assert_eq!(strtod(b"  0x1.8p1 rest").value, 3.0);
/// assert_eq!(strtod(b"  0x1.8p1 rest").len, 9);
/// assert_eq!(strtod(b"1e").len, 1, "an exponent needs digits");
/// assert!(strtod(b"1e400").out_of_range);
/// assert_eq!(strtod(b"x").len, 0);
/// ```
pub fn strtod(b: &[u8]) -> Scanned {
    let lead = b.iter().take_while(|&&c| is_c_space(c)).count();
    match scan(&b[lead..]) {
        None => Scanned { value: 0.0, len: 0, out_of_range: false },
        Some((lit, n)) => {
            let (value, out_of_range) = crate::round::to_f64(lit);
            Scanned { value, len: lead + n, out_of_range }
        }
    }
}

/// `b` as a double when all of it is one literal: no white space around
/// it, not NaN, and not out of the double's range. Infinities pass.
///
/// ```
/// use kevy_num::parse_exact;
/// assert_eq!(parse_exact(b"0x10"), Some(16.0));
/// assert_eq!(parse_exact(b"-inf"), Some(f64::NEG_INFINITY));
/// for bad in [&b" 1"[..], b"1 ", b"", b"nan", b"1e400", b"1e-400", b"1e"] {
///     assert_eq!(parse_exact(bad), None, "{:?}", bad);
/// }
/// ```
pub fn parse_exact(b: &[u8]) -> Option<f64> {
    if b.first().is_none_or(|&c| is_c_space(c)) {
        return None;
    }
    let s = strtod(b);
    (s.len == b.len() && !s.out_of_range && !s.value.is_nan()).then_some(s.value)
}

/// The literal at the very front of `b` (no white space) and its length.
pub(crate) fn scan(b: &[u8]) -> Option<(Literal<'_>, usize)> {
    let (negative, at) = match b.first() {
        Some(b'-') => (true, 1),
        Some(b'+') => (false, 1),
        _ => (false, 0),
    };
    let rest = &b[at..];
    let (kind, n) = named(rest).or_else(|| hex(rest)).or_else(|| decimal(rest))?;
    Some((Literal { negative, kind }, at + n))
}

fn named(b: &[u8]) -> Option<(Kind<'_>, usize)> {
    let starts = |w: &[u8]| b.len() >= w.len() && b[..w.len()].eq_ignore_ascii_case(w);
    if starts(b"infinity") {
        return Some((Kind::Infinity, 8));
    }
    if starts(b"inf") {
        return Some((Kind::Infinity, 3));
    }
    if !starts(b"nan") {
        return None;
    }
    // `nan(chars)` takes the parenthesised part only when it is closed
    let tail = &b[3..];
    if tail.first() == Some(&b'(') {
        let inner = tail[1..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b'_');
        let close = 1 + inner.count();
        if tail.get(close) == Some(&b')') {
            return Some((Kind::NotANumber, 3 + close + 1));
        }
    }
    Some((Kind::NotANumber, 3))
}

/// Digits of `radix` with at most one point; at least one digit.
fn mantissa(b: &[u8], hex: bool) -> Option<usize> {
    let digit = |c: u8| if hex { c.is_ascii_hexdigit() } else { c.is_ascii_digit() };
    let int = b.iter().take_while(|&&c| digit(c)).count();
    let mut n = int;
    let mut frac = 0;
    if b.get(n) == Some(&b'.') {
        frac = b[n + 1..].iter().take_while(|&&c| digit(c)).count();
        if int + frac > 0 {
            n += 1 + frac;
        }
    }
    (int + frac > 0).then_some(n)
}

/// An exponent marker, a sign and digits; `(0, 0)` when there are none.
fn exponent(b: &[u8], marker: u8) -> (i64, usize) {
    if !b.first().is_some_and(|c| c.eq_ignore_ascii_case(&marker)) {
        return (0, 0);
    }
    let (neg, at) = match b.get(1) {
        Some(b'-') => (true, 2),
        Some(b'+') => (false, 2),
        _ => (false, 1),
    };
    let digits = b[at..].iter().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return (0, 0);
    }
    // saturate: past this no literal is anything but zero or infinite
    let v =
        b[at..at + digits].iter().fold(0i64, |v, &c| (v * 10 + i64::from(c - b'0')).min(1 << 40));
    (if neg { -v } else { v }, at + digits)
}

fn decimal(b: &[u8]) -> Option<(Kind<'_>, usize)> {
    let n = mantissa(b, false)?;
    let (exp, e) = exponent(&b[n..], b'e');
    Some((Kind::Decimal { digits: &b[..n], exp, text: &b[..n + e] }, n + e))
}

fn hex(b: &[u8]) -> Option<(Kind<'_>, usize)> {
    if b.len() < 2 || b[0] != b'0' || !b[1].eq_ignore_ascii_case(&b'x') {
        return None;
    }
    // `0x` with no hex digit after it reads as the `0` alone
    let n = mantissa(&b[2..], true)?;
    let (exp, e) = exponent(&b[2 + n..], b'p');
    Some((Kind::Hex { digits: &b[2..2 + n], exp }, 2 + n + e))
}

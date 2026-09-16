//! C's number parsers, as redis-cli calls them on its options.
//!
//! redis-cli reads `-p` with `atoi`, `-r` with `strtoll`, `-i` with `atof`
//! and `-t` with `strtod`, and what it accepts is those functions' leniency:
//! leading blanks, a sign, digits up to the first non-digit, and 0 when
//! there are none. Using Rust's strict parsers instead would turn a value
//! redis-cli accepts into an error.

/// `atoi` / `strtol` base 10: blanks, sign, digits; saturates like glibc.
pub(crate) fn atoll(s: &[u8]) -> i64 {
    let (value, _) = strtoll(s);
    value
}

/// `strtoll(s, &end, 10)`: the value and how many bytes it consumed.
pub(crate) fn strtoll(s: &[u8]) -> (i64, usize) {
    let mut i = s.iter().take_while(|&&b| is_c_space(b)).count();
    let negative = matches!(s.get(i), Some(b'-'));
    if matches!(s.get(i), Some(b'-' | b'+')) {
        i += 1;
    }
    let start = i;
    let mut acc: i64 = 0;
    let mut saturated = false;
    while let Some(&d) = s.get(i).filter(|b| b.is_ascii_digit()) {
        let digit = i64::from(d - b'0');
        match acc
            .checked_mul(10)
            .and_then(|a| if negative { a.checked_sub(digit) } else { a.checked_add(digit) })
        {
            Some(next) => acc = next,
            None => saturated = true,
        }
        i += 1;
    }
    if i == start {
        return (0, 0);
    }
    if saturated {
        acc = if negative { i64::MIN } else { i64::MAX };
    }
    (acc, i)
}

/// `atoi`: `strtol` truncated to `int` the way glibc does it.
pub(crate) fn atoi(s: &[u8]) -> i32 {
    atoll(s) as i32
}

/// `strtod` requiring the whole string to be consumed; `None` otherwise.
pub(crate) fn strtod_full(s: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(s).ok()?;
    let trimmed = text.trim_start_matches(|c: char| c.is_ascii() && is_c_space(c as u8));
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<f64>().ok()
}

/// `atof`: the longest leading prefix that parses as a double, else 0.
pub(crate) fn atof(s: &[u8]) -> f64 {
    let text = String::from_utf8_lossy(s);
    let trimmed = text.trim_start_matches(|c: char| c.is_ascii() && is_c_space(c as u8));
    (1..=trimmed.len())
        .rev()
        .filter(|&n| trimmed.is_char_boundary(n))
        .find_map(|n| trimmed[..n].parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn is_c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atoi_is_lenient() {
        assert_eq!(atoi(b"6379"), 6379);
        assert_eq!(atoi(b"  -12abc"), -12);
        assert_eq!(atoi(b"abc"), 0);
        assert_eq!(atoi(b"+7"), 7);
    }

    #[test]
    fn strtoll_reports_consumption_and_saturates() {
        assert_eq!(strtoll(b"42x"), (42, 2));
        assert_eq!(strtoll(b"x"), (0, 0));
        assert_eq!(strtoll(b"99999999999999999999").0, i64::MAX);
        assert_eq!(strtoll(b"-99999999999999999999").0, i64::MIN);
    }

    #[test]
    fn doubles() {
        assert_eq!(strtod_full(b"1.5"), Some(1.5));
        assert_eq!(strtod_full(b"1.5s"), None);
        assert_eq!(strtod_full(b""), None);
        assert_eq!(atof(b"0.25sec"), 0.25);
        assert_eq!(atof(b"nope"), 0.0);
    }
}

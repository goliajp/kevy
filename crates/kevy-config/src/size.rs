//! Human-readable byte size literal parser: `"64mb"` → `64 * 1024 * 1024`.
//!
//! Accepts:
//! - Bare integers (interpreted as bytes): `"512"` → 512
//! - Suffix `k` / `kb` / `m` / `mb` / `g` / `gb` / `t` / `tb` (case-insensitive,
//!   binary multipliers — 1 KB = 1024 bytes, matching Redis convention)
//! - Optional whitespace between the number and the suffix: `"64 mb"`
//!
//! Rejects:
//! - Floating-point (`"1.5gb"`) — too ambiguous; users should write `1536mb`
//! - Negative numbers
//! - Empty strings

/// Parse a size literal (`"64mb"`, `"2gb"`, `"512"`, …) into a byte count.
///
/// Returns `Err` with the offending input on parse failure.
pub fn parse_size(input: &str) -> Result<u64, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err(format!("empty size literal: {input:?}"));
    }
    // Split into numeric prefix + unit suffix.
    let split_at = s
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(s.len());
    let (num_part, unit_part) = s.split_at(split_at);
    if num_part.is_empty() {
        return Err(format!("size literal {input:?} has no number"));
    }
    let n: u64 = num_part
        .parse()
        .map_err(|_| format!("size literal {input:?} has invalid number: {num_part:?}"))?;
    let multiplier = parse_unit(unit_part.trim())
        .ok_or_else(|| format!("size literal {input:?} has unknown unit: {unit_part:?}"))?;
    n.checked_mul(multiplier)
        .ok_or_else(|| format!("size literal {input:?} overflows u64"))
}

fn parse_unit(s: &str) -> Option<u64> {
    // Case-insensitive match, supporting both short and `b`-suffixed forms.
    match s.to_ascii_lowercase().as_str() {
        "" | "b" => Some(1),
        "k" | "kb" => Some(1024),
        "m" | "mb" => Some(1024 * 1024),
        "g" | "gb" => Some(1024 * 1024 * 1024),
        "t" | "tb" => Some(1024 * 1024 * 1024 * 1024),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_integer_is_bytes() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("1024").unwrap(), 1024);
    }

    #[test]
    fn binary_multipliers_match_redis() {
        assert_eq!(parse_size("1k").unwrap(), 1024);
        assert_eq!(parse_size("1kb").unwrap(), 1024);
        assert_eq!(parse_size("1KB").unwrap(), 1024);
        assert_eq!(parse_size("64mb").unwrap(), 64 * 1024 * 1024);
        assert_eq!(parse_size("2gb").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("1tb").unwrap(), 1024_u64.pow(4));
    }

    #[test]
    fn whitespace_tolerated() {
        assert_eq!(parse_size(" 64 mb ").unwrap(), 64 * 1024 * 1024);
        assert_eq!(parse_size("64 mb").unwrap(), 64 * 1024 * 1024);
    }

    #[test]
    fn empty_or_no_number_rejected() {
        assert!(parse_size("").is_err());
        assert!(parse_size("  ").is_err());
        assert!(parse_size("mb").is_err());
    }

    #[test]
    fn unknown_unit_rejected() {
        assert!(parse_size("64xb").is_err());
        assert!(parse_size("64 zz").is_err());
    }

    #[test]
    fn float_rejected() {
        // "1.5gb" splits at '.' which isn't a digit; num_part = "1", unit = ".5gb"
        // → unit parse fails. Good — user must write 1536mb.
        assert!(parse_size("1.5gb").is_err());
    }

    #[test]
    fn overflow_reported() {
        // 2^54 * 1024 (KB) = 2^64, overflows
        let huge = format!("{}kb", u64::MAX / 1024 + 1);
        assert!(parse_size(&huge).is_err());
    }
}

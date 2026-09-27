//! Byte counts for people: `1003B`, `4.03K`, `100.00M`, 1024-based.

/// `n` bytes as the report prints them: below 1024 the whole number, above
/// it two decimals in the largest unit reached, up to T.
pub(crate) fn bytes(n: u64) -> String {
    const UNITS: [(u64, &str); 4] =
        [(1 << 40, "T"), (1 << 30, "G"), (1 << 20, "M"), (1 << 10, "K")];
    for (size, unit) in UNITS {
        if n >= size {
            return format!("{:.2}{unit}", n as f64 / size as f64);
        }
    }
    format!("{n}B")
}

#[cfg(test)]
mod tests {
    use super::bytes;

    #[test]
    fn units() {
        assert_eq!(bytes(1003), "1003B");
        assert_eq!(bytes(4126), "4.03K");
        assert_eq!(bytes(104_857_600), "100.00M");
        assert_eq!(bytes(2_491_081_032), "2.32G");
        assert_eq!(bytes(3_309_450_000_000), "3.01T");
        assert_eq!(bytes(0), "0B");
    }
}

//! Bounded edit distance for typo-tolerant matching.
//!
//! Distance is measured in BYTES, which equals characters for the ASCII
//! and Latin text typo tolerance is for. A multi-byte character therefore
//! counts as its byte length — substituting one CJK ideograph is three
//! byte-edits, not one — so a typo budget is a Latin-text affordance, not
//! a CJK one. Stated rather than silently approximated.

/// Levenshtein distance between `a` and `b`, or `None` once it exceeds
/// `max`.
///
/// Two-row DP that abandons a row as soon as every cell in it is already
/// over budget, and rejects on the length difference first — a dictionary
/// scan spends almost nothing on the terms that cannot match.
pub(crate) fn edit_within(a: &[u8], b: &[u8], max: u32) -> Option<u32> {
    if a.len().abs_diff(b.len()) > max as usize {
        return None;
    }
    if a == b {
        return Some(0);
    }
    let lb = b.len();
    let mut prev: Vec<u32> = (0..=lb as u32).collect();
    let mut cur = vec![0u32; lb + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i as u32 + 1;
        let mut row_min = cur[0];
        for j in 1..=lb {
            let cost = u32::from(ca != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            row_min = row_min.min(cur[j]);
        }
        if row_min > max {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    (prev[lb] <= max).then_some(prev[lb])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances_within_budget() {
        assert_eq!(edit_within(b"quick", b"quick", 2), Some(0));
        assert_eq!(edit_within(b"quick", b"quik", 2), Some(1), "deletion");
        assert_eq!(edit_within(b"quick", b"quicks", 2), Some(1), "insertion");
        assert_eq!(edit_within(b"quick", b"qvick", 2), Some(1), "substitution");
        assert_eq!(edit_within(b"quick", b"qvik", 2), Some(2));
        assert_eq!(edit_within(b"", b"ab", 2), Some(2));
    }

    #[test]
    fn over_budget_is_none() {
        assert_eq!(edit_within(b"quick", b"slow", 2), None);
        assert_eq!(edit_within(b"quick", b"quicksilver", 2), None, "length gap alone rejects");
        assert_eq!(edit_within(b"quick", b"qvik", 1), None, "distance 2 over a budget of 1");
        assert_eq!(edit_within(b"abc", b"xyz", 2), None);
    }

    #[test]
    fn zero_budget_is_exact_match_only() {
        assert_eq!(edit_within(b"quick", b"quick", 0), Some(0));
        assert_eq!(edit_within(b"quick", b"quik", 0), None);
    }
}

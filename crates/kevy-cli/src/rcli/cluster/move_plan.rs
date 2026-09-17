//! Which slots a reshard takes from each source: in proportion to what each
//! source owns, largest source first.

/// `(source index, slot)` in the order they are moved.
pub(crate) fn plan(sources: &[(usize, Vec<u16>)], wanted: usize) -> Vec<(usize, u16)> {
    let total: usize = sources.iter().map(|(_, slots)| slots.len()).sum();
    if total == 0 {
        return Vec::new();
    }
    let mut order: Vec<&(usize, Vec<u16>)> = sources.iter().collect();
    order.sort_by_key(|(_, slots)| std::cmp::Reverse(slots.len()));
    let mut moves = Vec::new();
    for (k, (source, slots)) in order.into_iter().enumerate() {
        let share = wanted as f64 * slots.len() as f64 / total as f64;
        // The first source rounds up, so rounding never leaves a slot behind.
        let n = if k == 0 { share.ceil() } else { share.floor() } as usize;
        moves.extend(slots.iter().take(n).map(|&s| (*source, s)));
    }
    moves
}

#[cfg(test)]
mod tests {
    use super::plan;

    #[test]
    fn slots_come_from_sources_in_proportion() {
        let big: Vec<u16> = (10..=20).chain(10923..16384).collect();
        let small: Vec<u16> = (0..10).chain(21..5461).collect();
        let sources = [(0, small.clone()), (2, big.clone())];
        let moves = plan(&sources, 7);
        assert_eq!(moves, [(2, 10), (2, 11), (2, 12), (2, 13), (0, 0), (0, 1), (0, 2)]);
        assert_eq!(plan(&sources, 1), [(2, 10)]);
        assert_eq!(plan(&sources, 10930).len(), 10922);
        let even = [(0, (0..5461).collect::<Vec<u16>>()), (2, (10923..16384).collect())];
        assert_eq!(plan(&even, 5).iter().map(|m| m.0).collect::<Vec<_>>(), [0, 0, 0, 2, 2]);
        assert!(plan(&[(1, Vec::new())], 3).is_empty());
    }
}

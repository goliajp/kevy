//! Incremental history search (Ctrl-R backward, Ctrl-S forward).

use super::history::History;

/// Which way a search walks history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Backward,
    Forward,
}

/// The prompt shown while searching.
pub(crate) fn prompt(dir: Direction) -> &'static [u8] {
    match dir {
        Direction::Backward => b"(reverse-i-search): ",
        Direction::Forward => b"(i-search): ",
    }
}

/// A match: the history entry (counted back from the newest) and where the
/// term sits in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) back: usize,
    pub(crate) start: usize,
}

/// The first entry containing `term`, walking from the newest (backward) or
/// the oldest (forward) — or, when `after` is a previous match, from that
/// entry on, skipping entries whose text equals it so that cycling always
/// shows something new.
pub(crate) fn find(
    history: &History,
    term: &[u8],
    dir: Direction,
    after: Option<Found>,
) -> Option<Found> {
    let n = history.len();
    if n == 0 || term.is_empty() {
        return None;
    }
    let previous = after.and_then(|f| history.newest(f.back));
    let mut back = match (after, dir) {
        (Some(f), _) => f.back,
        (None, Direction::Backward) => 0,
        (None, Direction::Forward) => n - 1,
    };
    loop {
        let line = history.newest(back)?;
        let same_as_previous = previous.is_some_and(|p| p == line);
        if !same_as_previous && let Some(start) = line.windows(term.len()).position(|w| w == term) {
            return Some(Found { back, start });
        }
        back = match dir {
            Direction::Backward if back + 1 < n => back + 1,
            Direction::Forward if back > 0 => back - 1,
            _ => return None,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(lines: &[&str]) -> History {
        let mut h = History::default();
        for l in lines {
            h.add(l.as_bytes(), false);
        }
        h
    }

    #[test]
    fn backward_forward_and_cycling() {
        let h = history(&["SET a 1", "GET a", "SET b 2", "GET b"]);
        let first = find(&h, b"SET", Direction::Backward, None);
        assert_eq!(first, Some(Found { back: 1, start: 0 }));
        let next = find(&h, b"SET", Direction::Backward, first);
        assert_eq!(next, Some(Found { back: 3, start: 0 }));
        assert_eq!(find(&h, b"SET", Direction::Backward, next), None);
        assert_eq!(find(&h, b"b", Direction::Forward, None), Some(Found { back: 1, start: 4 }));
        assert_eq!(find(&h, b"GET", Direction::Forward, Some(Found { back: 0, start: 0 })), None);
        assert_eq!(find(&h, b"", Direction::Backward, None), None);
        assert_eq!(find(&History::default(), b"x", Direction::Backward, None), None);
        assert_eq!(
            (prompt(Direction::Backward), prompt(Direction::Forward)),
            (&b"(reverse-i-search): "[..], &b"(i-search): "[..])
        );
    }

    #[test]
    fn cycling_skips_an_identical_line() {
        let h = history(&["PING", "ECHO x", "PING"]);
        let first = find(&h, b"PING", Direction::Backward, None);
        assert_eq!(first, Some(Found { back: 0, start: 0 }));
        assert_eq!(
            find(&h, b"PING", Direction::Backward, first),
            None,
            "the older PING reads the same"
        );
    }
}

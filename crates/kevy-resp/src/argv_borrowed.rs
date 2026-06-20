//! Zero-copy argv that borrows directly from the reactor's read buffer.
//!
//! `ArgvBorrowed<'a>` records each parsed arg as a `(start, end)` range into a
//! caller-provided input slice — typically `&conn.input[..]`. The local single-
//! shard hot path can dispatch straight from these slices and skip the per-cmd
//! memcpy that [`crate::Argv`] needs. Handoff junctures (cross-shard dispatch,
//! the MULTI queue, AOF logging) call [`ArgvBorrowed::into_owned`] to materialise
//! a normal `Argv` and preserve current owned semantics there.

use crate::argv::Argv;
use crate::inline_ranges::InlineRanges;

/// A parsed command's argument vector that borrows its bytes from a contiguous
/// input buffer.
///
/// Unlike [`Argv`], which packs all argument bytes into a fresh `Vec<u8>`,
/// `ArgvBorrowed` stores only a `(start, end)` table over the original buffer.
/// `get(i)` returns `&input[s..e]`, so no copy happens on the parse → dispatch
/// path. Calls that need to outlive the buffer (cross-shard, MULTI queue, AOF)
/// use [`into_owned`](Self::into_owned) to convert to `Argv`.
///
/// A5 (2026-06-20): the range table is a `(u32, u32) × 4` inline + heap-spill
/// `InlineRanges`. Commands with ≤4 args (PING/GET/SET/INCR/MGET ≤4 keys —
/// the vast majority of the -c1 hot mix) pay zero `malloc`/`free` for the
/// ranges. H1 (`perf c2c`) confirmed libc cfree on the per-request `Vec`
/// allocation showed up in cross-thread contention; the inline tier removes
/// that source.
#[derive(Clone, Debug)]
pub struct ArgvBorrowed<'a> {
    input: &'a [u8],
    ranges: InlineRanges,
}

impl<'a> ArgvBorrowed<'a> {
    /// An empty argv that will read arg bytes from `input`.
    pub fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            ranges: InlineRanges::new(),
        }
    }

    /// An empty argv, pre-sizing `ranges` for `argc` args.
    pub fn with_capacity(input: &'a [u8], argc: usize) -> Self {
        Self {
            input,
            ranges: InlineRanges::with_capacity(argc),
        }
    }

    /// Record one argument as `input[start..end]`.
    pub(crate) fn push_range(&mut self, start: usize, end: usize) {
        debug_assert!(end <= self.input.len() && start <= end);
        self.ranges.push((start as u32, end as u32));
    }

    /// Number of arguments.
    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    /// Whether there are no arguments.
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Argument `i` as a byte slice into the original input, or `None`.
    pub fn get(&self, i: usize) -> Option<&[u8]> {
        let (s, e) = self.ranges.get(i)?;
        Some(&self.input[s as usize..e as usize])
    }

    /// The first argument (the command name), or `None` if empty.
    pub fn first(&self) -> Option<&[u8]> {
        self.get(0)
    }

    /// Iterate the arguments as byte slices into the original input.
    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        (0..self.len()).map(move |i| self.get(i).expect("in range"))
    }

    /// Materialise an owned [`Argv`] — copies arg bytes into a fresh buffer.
    /// Used at any handoff juncture (cross-shard dispatch, MULTI queue, AOF
    /// logging) that needs to outlive the original input buffer.
    pub fn into_owned(self) -> Argv {
        let mut total: usize = 0;
        for i in 0..self.ranges.len() {
            let (s, e) = self.ranges.get(i).expect("in range");
            total += (e - s) as usize;
        }
        let mut a = Argv::with_capacity(self.ranges.len(), total);
        for i in 0..self.ranges.len() {
            let (s, e) = self.ranges.get(i).expect("in range");
            a.push(&self.input[s as usize..e as usize]);
        }
        a
    }
}

impl core::ops::Index<usize> for ArgvBorrowed<'_> {
    type Output = [u8];
    fn index(&self, i: usize) -> &[u8] {
        self.get(i).expect("argv-borrowed index out of bounds")
    }
}

impl PartialEq<Vec<Vec<u8>>> for ArgvBorrowed<'_> {
    fn eq(&self, other: &Vec<Vec<u8>>) -> bool {
        self.len() == other.len() && self.iter().zip(other).all(|(a, b)| a == b.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_and_with_capacity_start_empty() {
        let buf = b"hello";
        let a = ArgvBorrowed::new(buf);
        assert!(a.is_empty());
        assert_eq!(a.len(), 0);
        let b = ArgvBorrowed::with_capacity(buf, 8);
        assert!(b.is_empty());
    }

    #[test]
    fn push_range_and_get_round_trip() {
        // input buffer carrying interleaved args + headers, like RESP2 multibulk
        let buf: &[u8] = b"*2\r\n$3\r\nGET\r\n$5\r\nmykey\r\n";
        let mut a = ArgvBorrowed::with_capacity(buf, 2);
        // GET at offset 8..11, mykey at 17..22 (right after `\r\n$5\r\n`)
        a.push_range(8, 11);
        a.push_range(17, 22);
        assert_eq!(a.len(), 2);
        assert_eq!(a.first(), Some(b"GET" as &[u8]));
        assert_eq!(a.get(0), Some(b"GET" as &[u8]));
        assert_eq!(a.get(1), Some(b"mykey" as &[u8]));
        assert_eq!(a.get(2), None);
    }

    #[test]
    fn iter_yields_args_in_order() {
        let buf: &[u8] = b"abcXYZdef";
        let mut a = ArgvBorrowed::new(buf);
        a.push_range(0, 3);
        a.push_range(3, 6);
        a.push_range(6, 9);
        let collected: Vec<&[u8]> = a.iter().collect();
        assert_eq!(collected, vec![b"abc" as &[u8], b"XYZ", b"def"]);
    }

    #[test]
    fn first_empty_returns_none() {
        let a = ArgvBorrowed::new(b"" as &[u8]);
        assert_eq!(a.first(), None);
        assert_eq!(a.get(0), None);
    }

    #[test]
    fn index_returns_correct_slice() {
        let buf: &[u8] = b"hithere";
        let mut a = ArgvBorrowed::new(buf);
        a.push_range(0, 2);
        a.push_range(2, 7);
        assert_eq!(&a[0], b"hi" as &[u8]);
        assert_eq!(&a[1], b"there" as &[u8]);
    }

    #[test]
    #[should_panic(expected = "argv-borrowed index out of bounds")]
    fn index_out_of_bounds_panics() {
        let a = ArgvBorrowed::new(b"" as &[u8]);
        let _ = &a[0];
    }

    #[test]
    fn eq_against_vec_of_vec() {
        let buf: &[u8] = b"PINGhello";
        let mut a = ArgvBorrowed::new(buf);
        a.push_range(0, 4);
        a.push_range(4, 9);
        assert_eq!(a, vec![b"PING".to_vec(), b"hello".to_vec()]);
        assert_ne!(a, vec![b"PING".to_vec()]);
        assert_ne!(a, vec![b"PING".to_vec(), b"world".to_vec()]);
    }

    #[test]
    fn into_owned_copies_args_into_argv() {
        // Non-contiguous in the original buffer (interleaved with RESP markers).
        let buf: &[u8] = b"*2\r\n$3\r\nSET\r\n$1\r\nk\r\n";
        let mut a = ArgvBorrowed::with_capacity(buf, 2);
        a.push_range(8, 11); // SET
        a.push_range(17, 18); // k
        let owned: Argv = a.into_owned();
        assert_eq!(owned.len(), 2);
        assert_eq!(owned.get(0), Some(b"SET" as &[u8]));
        assert_eq!(owned.get(1), Some(b"k" as &[u8]));
        // And the materialised Argv compares equal to the vec-of-vec form.
        assert_eq!(owned, vec![b"SET".to_vec(), b"k".to_vec()]);
    }

    #[test]
    fn into_owned_on_empty_argv_returns_empty_argv() {
        let a = ArgvBorrowed::new(b"" as &[u8]);
        let owned = a.into_owned();
        assert!(owned.is_empty());
        assert_eq!(owned.len(), 0);
    }

    #[test]
    fn clone_shares_input_slice_independent_ranges() {
        let buf: &[u8] = b"abcdef";
        let mut a = ArgvBorrowed::new(buf);
        a.push_range(0, 3); // abc
        let b = a.clone();
        assert_eq!(b.len(), 1);
        assert_eq!(b.get(0), Some(b"abc" as &[u8]));
        // Mutating original's ranges doesn't affect clone.
        a.push_range(3, 6); // def
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 1);
    }
}

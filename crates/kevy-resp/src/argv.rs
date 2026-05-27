//! The parsed-command argument vector. Two-allocation layout so a SET command
//! drops from 4 mallocs (Vec of Vec) to 2 (one buffer, one offset table).

/// A parsed command's argument vector.
///
/// Stored in **two allocations** — all argument bytes concatenated in `buf`,
/// with `ends[i]` the end offset of argument `i` — instead of the `N+1` a
/// `Vec<Vec<u8>>` needs (one outer `Vec` plus one per argument). Parsing a SET
/// drops from 4 allocations to 2. It is `Send` (two `Vec`s), so the
/// thread-per-core runtime still forwards it across cores by value.
///
/// Index/`get`/`first`/`iter` return `&[u8]` argument slices. It compares equal
/// to a `Vec<Vec<u8>>` of the same arguments, so call sites and tests read
/// naturally.
#[derive(Clone, Default, Debug, Eq)]
pub struct Argv {
    buf: Vec<u8>,
    ends: Vec<u32>,
}

impl Argv {
    /// An empty argv, pre-sizing for `argc` args totalling `bytes` bytes.
    pub fn with_capacity(argc: usize, bytes: usize) -> Self {
        Argv {
            buf: Vec::with_capacity(bytes),
            ends: Vec::with_capacity(argc),
        }
    }

    /// Drop all args while keeping the buf + ends capacity. Used by the
    /// reactor's per-command scratch `Argv`: `parse_command_into` clears
    /// then refills, so the hot path's malloc rate drops to ~0.
    #[inline]
    pub fn clear(&mut self) {
        self.buf.clear();
        self.ends.clear();
    }

    /// Reserve room for `argc` args totalling `bytes` bytes on top of what is
    /// already there (no shrink).
    #[inline]
    pub fn reserve_for(&mut self, argc: usize, bytes: usize) {
        self.buf.reserve(bytes);
        self.ends.reserve(argc);
    }

    /// Append one argument.
    pub fn push(&mut self, arg: &[u8]) {
        self.buf.extend_from_slice(arg);
        self.ends.push(self.buf.len() as u32);
    }

    /// Number of arguments.
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Whether there are no arguments.
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Argument `i` as a byte slice, or `None` if out of range.
    pub fn get(&self, i: usize) -> Option<&[u8]> {
        let end = *self.ends.get(i)? as usize;
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        Some(&self.buf[start..end])
    }

    /// The first argument (the command name), or `None` if empty.
    pub fn first(&self) -> Option<&[u8]> {
        self.get(0)
    }

    /// Iterate the arguments as byte slices.
    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        (0..self.len()).map(move |i| self.get(i).expect("in range"))
    }
}

impl core::ops::Index<usize> for Argv {
    type Output = [u8];
    fn index(&self, i: usize) -> &[u8] {
        self.get(i).expect("argv index out of bounds")
    }
}

/// Compare to a `Vec<Vec<u8>>` of the same arguments (keeps call sites + tests
/// that build the expected value as a vec-of-vecs readable).
impl PartialEq<Vec<Vec<u8>>> for Argv {
    fn eq(&self, other: &Vec<Vec<u8>>) -> bool {
        self.len() == other.len() && self.iter().zip(other).all(|(a, b)| a == b.as_slice())
    }
}

impl PartialEq for Argv {
    fn eq(&self, other: &Argv) -> bool {
        self.buf == other.buf && self.ends == other.ends
    }
}

/// Build from a vec-of-vecs (test/embedding convenience; the wire path uses
/// [`parse_command`](crate::parse_command), which builds an [`Argv`] directly
/// without the intermediate allocations).
impl From<Vec<Vec<u8>>> for Argv {
    fn from(v: Vec<Vec<u8>>) -> Self {
        let mut a = Argv::with_capacity(v.len(), v.iter().map(Vec::len).sum());
        for arg in &v {
            a.push(arg);
        }
        a
    }
}

/// A parsed command: `argv`, where `argv[0]` is the command name.
pub type Command = Argv;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_capacity_preallocates_both_buffers() {
        let a: Argv = Argv::with_capacity(4, 32);
        assert_eq!(a.len(), 0);
        assert!(a.is_empty());
        // Underlying Vecs reserve at least what we asked for (no shrink).
        assert!(a.buf.capacity() >= 32);
        assert!(a.ends.capacity() >= 4);
    }

    #[test]
    fn clear_keeps_capacity_resets_len() {
        let mut a = Argv::default();
        a.push(b"foo");
        a.push(b"barbaz");
        assert_eq!(a.len(), 2);
        let cap_buf = a.buf.capacity();
        let cap_ends = a.ends.capacity();
        a.clear();
        assert_eq!(a.len(), 0);
        assert!(a.is_empty());
        // Capacities preserved (the reuse contract the hot reactor relies on).
        assert_eq!(a.buf.capacity(), cap_buf);
        assert_eq!(a.ends.capacity(), cap_ends);
    }

    #[test]
    fn reserve_for_grows_to_at_least_requested() {
        let mut a = Argv::with_capacity(1, 4);
        a.reserve_for(8, 64);
        assert!(a.buf.capacity() >= 64);
        assert!(a.ends.capacity() >= 8);
        // reserve_for never shrinks.
        a.reserve_for(0, 0);
        assert!(a.buf.capacity() >= 64);
        assert!(a.ends.capacity() >= 8);
    }

    #[test]
    fn push_get_iter_first_round_trip() {
        let mut a = Argv::default();
        a.push(b"SET");
        a.push(b"key");
        a.push(b"value");
        assert_eq!(a.len(), 3);
        assert_eq!(a.first(), Some(b"SET" as &[u8]));
        assert_eq!(a.get(0), Some(b"SET" as &[u8]));
        assert_eq!(a.get(1), Some(b"key" as &[u8]));
        assert_eq!(a.get(2), Some(b"value" as &[u8]));
        assert_eq!(a.get(3), None);
        let collected: Vec<&[u8]> = a.iter().collect();
        assert_eq!(collected, vec![b"SET" as &[u8], b"key", b"value"]);
    }

    #[test]
    fn first_on_empty_returns_none() {
        let a = Argv::default();
        assert_eq!(a.first(), None);
        assert_eq!(a.get(0), None);
    }

    #[test]
    fn index_returns_correct_slice() {
        let mut a = Argv::default();
        a.push(b"hi");
        a.push(b"there");
        assert_eq!(&a[0], b"hi" as &[u8]);
        assert_eq!(&a[1], b"there" as &[u8]);
    }

    #[test]
    #[should_panic(expected = "argv index out of bounds")]
    fn index_out_of_bounds_panics() {
        let a = Argv::default();
        let _ = &a[0];
    }

    #[test]
    fn eq_against_vec_of_vec() {
        let mut a = Argv::default();
        a.push(b"PING");
        a.push(b"hello");
        assert_eq!(a, vec![b"PING".to_vec(), b"hello".to_vec()]);
        assert_ne!(a, vec![b"PING".to_vec()]);
        assert_ne!(a, vec![b"PING".to_vec(), b"world".to_vec()]);
    }

    #[test]
    fn eq_argv_vs_argv() {
        let mut a = Argv::default();
        a.push(b"A");
        a.push(b"B");
        let mut b = Argv::default();
        b.push(b"A");
        b.push(b"B");
        let mut c = Argv::default();
        c.push(b"A");
        c.push(b"C");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn from_vec_of_vec_preserves_args() {
        let v = vec![b"GET".to_vec(), b"my-key".to_vec()];
        let a: Argv = Argv::from(v.clone());
        assert_eq!(a.len(), 2);
        assert_eq!(a, v);
        // From<Vec<Vec<u8>>> reserves exactly the right total size.
        assert_eq!(a.buf.len(), 3 + 6);
    }

    #[test]
    fn clone_makes_independent_argv() {
        let mut a = Argv::default();
        a.push(b"X");
        let b = a.clone();
        assert_eq!(a, b);
        // mutating original doesn't affect clone.
        a.push(b"Y");
        assert_ne!(a, b);
        assert_eq!(b.len(), 1);
    }
}

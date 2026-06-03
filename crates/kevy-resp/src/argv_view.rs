//! A read-only argv abstraction shared by [`crate::Argv`] (owned) and
//! [`crate::ArgvBorrowed`] (zero-copy view into a buffer).
//!
//! `ArgvView` is the trait the command runtime takes by generic — every verb
//! and routing decision works against this trait so the reactor's local
//! single-shard hot path can dispatch directly from a borrowed argv, while
//! cross-shard / MULTI queue / AOF logging materialise an owned `Argv` at the
//! handoff juncture via `into_owned()`.
//!
//! Index access (`args[i]`) is part of the contract via the `Index<usize,
//! Output = [u8]>` supertrait, so verb implementations keep the existing
//! `args[i]` / `args.iter()` / `args.first()` syntax across the switch.

use crate::argv::Argv;
use crate::argv_borrowed::ArgvBorrowed;

/// Read-only view over a parsed command's argument vector.
///
/// Implemented by both [`Argv`] (owned) and [`ArgvBorrowed`] (zero-copy). The
/// command runtime takes argvs as `&impl ArgvView`, so the local fast path
/// can hand a borrowed argv straight to dispatch with no memcpy.
pub trait ArgvView: core::ops::Index<usize, Output = [u8]> {
    /// Number of arguments.
    fn len(&self) -> usize;
    /// Argument `i` as a byte slice, or `None` if out of range.
    fn get(&self, i: usize) -> Option<&[u8]>;

    /// Whether there are no arguments.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The first argument (the command name), or `None` if empty.
    fn first(&self) -> Option<&[u8]> {
        self.get(0)
    }

    /// Iterate the arguments as byte slices.
    fn iter(&self) -> ArgvIter<'_, Self>
    where
        Self: Sized,
    {
        ArgvIter { view: self, i: 0 }
    }

    /// Materialise an owned [`Argv`] — copies arg bytes into a fresh buffer.
    /// Used at handoff junctures (cross-shard dispatch, MULTI queue, AOF
    /// logging) that need to outlive the original input buffer. Object-safe
    /// (no `Self: Sized` bound) so callers can hold `&dyn ArgvView`.
    fn to_argv(&self) -> Argv {
        let n = self.len();
        let total: usize = (0..n).map(|i| self.get(i).map_or(0, <[u8]>::len)).sum();
        let mut out = Argv::with_capacity(n, total);
        for i in 0..n {
            if let Some(arg) = self.get(i) {
                out.push(arg);
            }
        }
        out
    }
}

/// Iterator yielding each `ArgvView`'s arguments as `&[u8]` slices.
///
/// Returned by [`ArgvView::iter`]. Concrete (rather than `impl Iterator`) so
/// the method works for both `Argv` and `ArgvBorrowed` callers.
pub struct ArgvIter<'a, V: ?Sized + ArgvView> {
    view: &'a V,
    i: usize,
}

impl<'a, V: ?Sized + ArgvView> Iterator for ArgvIter<'a, V> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        let r = self.view.get(self.i);
        if r.is_some() {
            self.i += 1;
        }
        r
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let rem = self.view.len().saturating_sub(self.i);
        (rem, Some(rem))
    }
}

impl<V: ?Sized + ArgvView> ExactSizeIterator for ArgvIter<'_, V> {}

impl ArgvView for Argv {
    fn len(&self) -> usize {
        Argv::len(self)
    }
    fn get(&self, i: usize) -> Option<&[u8]> {
        Argv::get(self, i)
    }
}

impl ArgvView for ArgvBorrowed<'_> {
    fn len(&self) -> usize {
        ArgvBorrowed::len(self)
    }
    fn get(&self, i: usize) -> Option<&[u8]> {
        ArgvBorrowed::get(self, i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_arg<A: ArgvView>(a: &A) -> Option<&[u8]> {
        a.first()
    }

    fn arg_at<A: ArgvView>(a: &A, i: usize) -> &[u8] {
        &a[i]
    }

    fn collect_iter<A: ArgvView>(a: &A) -> Vec<Vec<u8>> {
        a.iter().map(|s| s.to_vec()).collect()
    }

    #[test]
    fn argv_implements_argv_view() {
        let mut a = Argv::default();
        a.push(b"SET");
        a.push(b"k");
        a.push(b"v");
        assert_eq!(ArgvView::len(&a), 3);
        assert_eq!(first_arg(&a), Some(b"SET" as &[u8]));
        assert_eq!(arg_at(&a, 1), b"k" as &[u8]);
        assert_eq!(
            collect_iter(&a),
            vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]
        );
    }

    #[test]
    fn argv_borrowed_implements_argv_view() {
        let buf: &[u8] = b"abcdef";
        let mut a = ArgvBorrowed::new(buf);
        a.push_range(0, 3); // abc
        a.push_range(3, 6); // def
        assert_eq!(ArgvView::len(&a), 2);
        assert_eq!(first_arg(&a), Some(b"abc" as &[u8]));
        assert_eq!(arg_at(&a, 1), b"def" as &[u8]);
        assert_eq!(collect_iter(&a), vec![b"abc".to_vec(), b"def".to_vec()]);
    }

    #[test]
    fn iter_size_hint_is_exact() {
        let mut a = Argv::default();
        a.push(b"A");
        a.push(b"B");
        a.push(b"C");
        let it = ArgvView::iter(&a);
        assert_eq!(it.size_hint(), (3, Some(3)));
        assert_eq!(it.len(), 3);
    }

    #[test]
    fn empty_argv_iter_yields_nothing() {
        let a = Argv::default();
        assert!(ArgvView::is_empty(&a));
        assert_eq!(first_arg(&a), None);
        let mut it = ArgvView::iter(&a);
        assert!(it.next().is_none());
    }

    #[test]
    fn generic_over_owned_and_borrowed_with_same_api() {
        // The point of ArgvView: verb code reads the same regardless of owner.
        fn route_name<A: ArgvView>(a: &A) -> &[u8] {
            a.first().unwrap_or(b"")
        }
        let mut owned = Argv::default();
        owned.push(b"PING");
        let buf: &[u8] = b"PING";
        let mut borrowed = ArgvBorrowed::new(buf);
        borrowed.push_range(0, 4);
        assert_eq!(route_name(&owned), b"PING");
        assert_eq!(route_name(&borrowed), b"PING");
    }
}

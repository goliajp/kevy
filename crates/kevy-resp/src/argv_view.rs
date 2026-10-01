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
///
/// Open for implementation: a transport or a log reader that already
/// holds a command's arguments in its own layout implements it to reach
/// the command runtime without copying them into an [`Argv`]. An
/// implementation must keep the three views of one argument vector in
/// agreement: `get(i)` is `Some` exactly for `i < len()`, and `self[i]`
/// returns the same bytes as `get(i)` (panicking past the end, as slice
/// indexing does). The provided methods rely on nothing else.
///
/// ```
/// use kevy_resp::ArgvView;
/// struct Pair<'a>(&'a [u8], &'a [u8]);
/// impl std::ops::Index<usize> for Pair<'_> {
///     type Output = [u8];
///     fn index(&self, i: usize) -> &[u8] {
///         self.get(i).expect("argument index in range")
///     }
/// }
/// impl ArgvView for Pair<'_> {
///     fn len(&self) -> usize {
///         2
///     }
///     fn get(&self, i: usize) -> Option<&[u8]> {
///         [self.0, self.1].get(i).copied()
///     }
/// }
/// let p = Pair(b"GET", b"k");
/// assert_eq!(p.first(), Some(&b"GET"[..]));
/// assert_eq!(p.to_argv().len(), 2);
/// ```
pub trait ArgvView: core::ops::Index<usize, Output = [u8]> {
    /// Number of arguments.
    ///
    /// ```
    /// use kevy_resp::{Argv, ArgvView};
    /// let argv = Argv::from(vec![b"MGET".to_vec(), b"a".to_vec(), b"b".to_vec()]);
    /// assert_eq!(ArgvView::len(&argv), 3);
    /// ```
    fn len(&self) -> usize;
    /// Argument `i` as a byte slice, or `None` if out of range.
    ///
    /// ```
    /// use kevy_resp::{Argv, ArgvView};
    /// let argv = Argv::from(vec![b"GET".to_vec(), b"k".to_vec()]);
    /// assert_eq!(ArgvView::get(&argv, 1), Some(b"k".as_slice()));
    /// assert_eq!(ArgvView::get(&argv, 2), None);
    /// ```
    fn get(&self, i: usize) -> Option<&[u8]>;

    /// Whether there are no arguments.
    ///
    /// ```
    /// use kevy_resp::{Argv, ArgvView};
    /// assert!(ArgvView::is_empty(&Argv::default()));
    /// assert!(!ArgvView::is_empty(&Argv::from(vec![b"PING".to_vec()])));
    /// ```
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The first argument (the command name), or `None` if empty.
    ///
    /// ```
    /// use kevy_resp::{ArgvView, parse_command_borrowed};
    /// let (argv, _) = parse_command_borrowed(b"PING\r\n")?.expect("complete frame");
    /// assert_eq!(argv.first(), Some(b"PING".as_slice()));
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    fn first(&self) -> Option<&[u8]> {
        self.get(0)
    }

    /// Iterate the arguments as byte slices.
    ///
    /// ```
    /// use kevy_resp::{ArgvView, parse_command_borrowed};
    /// let (argv, _) = parse_command_borrowed(b"DEL a b\r\n")?.expect("complete frame");
    /// let keys: Vec<&[u8]> = argv.iter().skip(1).collect();
    /// assert_eq!(keys, [b"a".as_slice(), b"b"]);
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    fn iter(&self) -> ArgvIter<'_, Self>
    where
        Self: Sized,
    {
        ArgvIter { view: self, i: 0 }
    }

    /// Clear `out` and refill it with this view's arguments. `out` keeps
    /// its buffer capacity across the clear, so refilling a recycled
    /// [`Argv`] (see [`crate::ArgvPool`]) is allocation-free in steady
    /// state. Object-safe (no `Self: Sized` bound).
    ///
    /// ```
    /// use kevy_resp::{Argv, ArgvView, parse_command_borrowed};
    /// let (argv, _) = parse_command_borrowed(b"GET k\r\n")?.expect("complete frame");
    /// let mut scratch = Argv::from(vec![b"stale".to_vec()]);
    /// argv.copy_into(&mut scratch);
    /// assert_eq!(scratch, vec![b"GET".to_vec(), b"k".to_vec()]);
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    fn copy_into(&self, out: &mut Argv) {
        out.clear();
        let n = self.len();
        let total: usize = (0..n).map(|i| self.get(i).map_or(0, <[u8]>::len)).sum();
        out.reserve_for(n, total);
        for i in 0..n {
            if let Some(arg) = self.get(i) {
                out.push(arg);
            }
        }
    }

    /// Materialise an owned [`Argv`] — copies arg bytes into a fresh buffer.
    /// Used at handoff junctures (cross-shard dispatch, MULTI queue, AOF
    /// logging) that need to outlive the original input buffer. Object-safe
    /// (no `Self: Sized` bound) so callers can hold `&dyn ArgvView`.
    ///
    /// ```
    /// use kevy_resp::{Argv, ArgvView, parse_command_borrowed};
    /// let input = b"SET k v\r\n".to_vec();
    /// let owned: Argv = {
    ///     let (argv, _) = parse_command_borrowed(&input)?.expect("complete frame");
    ///     let view: &dyn ArgvView = &argv;
    ///     view.to_argv()
    /// };
    /// drop(input);
    /// assert_eq!(owned.len(), 3);
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    fn to_argv(&self) -> Argv {
        let mut out = Argv::default();
        self.copy_into(&mut out);
        out
    }
}

/// Iterator yielding each `ArgvView`'s arguments as `&[u8]` slices.
///
/// Returned by [`ArgvView::iter`]. Concrete (rather than `impl Iterator`) so
/// the method works for both `Argv` and `ArgvBorrowed` callers.
///
/// ```
/// use kevy_resp::{ArgvView, parse_command_borrowed};
/// let (argv, _) = parse_command_borrowed(b"SADD s x y\r\n")?.expect("complete frame");
/// let mut it = ArgvView::iter(&argv);
/// assert_eq!(it.len(), 4);
/// assert_eq!(it.next(), Some(b"SADD".as_slice()));
/// assert_eq!(it.len(), 3);
/// # Ok::<(), kevy_resp::ProtocolError>(())
/// ```
#[derive(Debug)]
pub struct ArgvIter<'a, V: ?Sized> {
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
    #[inline]
    fn len(&self) -> usize {
        Argv::len(self)
    }
    #[inline]
    fn get(&self, i: usize) -> Option<&[u8]> {
        Argv::get(self, i)
    }
}

impl ArgvView for ArgvBorrowed<'_> {
    #[inline]
    fn len(&self) -> usize {
        ArgvBorrowed::len(self)
    }
    #[inline]
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
        a.iter().map(<[u8]>::to_vec).collect()
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
        assert_eq!(collect_iter(&a), vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
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

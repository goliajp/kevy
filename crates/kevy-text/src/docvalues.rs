//! Doc values — a document's *own* field values, stored column-wise
//! beside the postings.
//!
//! Everything else in this crate maps **term → documents**: that is what
//! ranking needs. `FILTER`, `SORT`, `DISTINCT` and `FACET` all need the
//! opposite direction — **document → its own value** — which an inverted
//! index cannot answer without walking every posting list of every
//! possible value. So an index that declares value fields keeps them
//! here, and a query that reads one gets an indexed lookup rather than a
//! scan. Same idea, and the same name, as Lucene's doc values.
//!
//! Values are raw bytes: this crate does not know what a number or a date
//! is, and should not. A predicate arrives as a test over the bytes, so
//! typing and coercion stay with the caller that already owns them.
//!
//! Like [`crate::positions`] and [`crate::fields`], the channel exists
//! only when the index declared it, and a query that does not read it
//! never touches it.

/// One stored value.
///
/// Filterable values are overwhelmingly short — a price, a status, a
/// category, an id — so a value up to 23 bytes lives inline and costs no
/// allocation at all. That is the same trade [`crate::buckets::Buckets`]
/// and [`crate::docblobs::DocBlobs`] make with their `One` variants,
/// applied to the value column: the enum is 32 bytes either way, so
/// inlining is free of size and saves an allocation per document per
/// field.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) enum Val {
    /// The document has no value for this field. Not the same as an
    /// empty value: a predicate never passes on an absent one.
    #[default]
    Absent,
    Inline { len: u8, buf: [u8; 23] },
    Heap(Vec<u8>),
}

impl Val {
    fn store(v: &[u8]) -> Val {
        if v.len() <= 23 {
            let mut buf = [0u8; 23];
            buf[..v.len()].copy_from_slice(v);
            Val::Inline { len: v.len() as u8, buf }
        } else {
            Val::Heap(v.to_vec())
        }
    }

    fn bytes(&self) -> Option<&[u8]> {
        match self {
            Val::Absent => None,
            Val::Inline { len, buf } => Some(&buf[..*len as usize]),
            Val::Heap(v) => Some(v),
        }
    }

    /// Heap bytes beyond the slot itself.
    fn heap(&self) -> u64 {
        match self {
            Val::Heap(v) => (v.len().max(1) as u64).next_multiple_of(16) + 16,
            _ => 0,
        }
    }
}

/// The stored-value side-channel: document id → its declared values.
#[derive(Debug)]
pub(crate) struct DocValues {
    /// How many value fields the index declares — the stride of `vals`.
    n: usize,
    /// id → its values, flat with stride `n`, so a document costs no
    /// allocation of its own and a lookup is one index.
    vals: Vec<Val>,
    /// Running Σ of every slot's spill heap — maintained per slot
    /// write (v4.1-V5), so the memory formula never walks the column.
    heap_bytes: u64,
}

impl DocValues {
    pub(crate) fn new(n: usize) -> Self {
        Self { n, vals: Vec::new(), heap_bytes: 0 }
    }

    /// Store `id`'s values, growing the flat table as ids are handed out.
    /// A shorter slice than the declared arity leaves the rest absent.
    pub(crate) fn set(&mut self, id: u32, values: &[Option<&[u8]>]) {
        let base = id as usize * self.n;
        if self.vals.len() < base + self.n {
            self.vals.resize(base + self.n, Val::Absent);
        }
        for f in 0..self.n {
            let new = match values.get(f).copied().flatten() {
                Some(v) => Val::store(v),
                None => Val::Absent,
            };
            self.heap_bytes += new.heap();
            self.heap_bytes -= self.vals[base + f].heap();
            self.vals[base + f] = new;
        }
    }

    /// Forget `id`'s values — the withdrawal counterpart of
    /// [`DocValues::set`], so a reused id slot never inherits them.
    pub(crate) fn clear(&mut self, id: u32) {
        self.set(id, &[]);
    }

    /// `id`'s value for `field`, or `None` when the document has none.
    pub(crate) fn get(&self, id: u32, field: usize) -> Option<&[u8]> {
        if field >= self.n {
            return None;
        }
        self.vals.get(id as usize * self.n + field)?.bytes()
    }

    /// How many value fields the segment stores.
    pub(crate) fn arity(&self) -> usize {
        self.n
    }

    /// Approximate heap bytes — the stored-value term of the memory
    /// formula. O(1): the slot table charges by capacity and the spill
    /// heap is a running counter (v4.1-V5).
    pub(crate) fn approx_bytes(&self) -> u64 {
        self.vals.capacity() as u64 * std::mem::size_of::<Val>() as u64 + self.heap_bytes
    }

    /// The walking reference for the invariant test.
    #[cfg(test)]
    pub(crate) fn recompute_bytes(&self) -> u64 {
        self.vals.capacity() as u64 * std::mem::size_of::<Val>() as u64
            + self.vals.iter().map(Val::heap).sum::<u64>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_slot_is_thirty_two_bytes_either_way() {
        // The inline buffer is sized to fill the space a `Vec` needs
        // anyway; if that stops being true the inline case has started
        // costing memory rather than saving allocations.
        assert_eq!(std::mem::size_of::<Val>(), 32);
    }

    #[test]
    fn short_values_stay_inline_long_ones_spill() {
        assert!(matches!(Val::store(b"42"), Val::Inline { .. }));
        assert!(matches!(Val::store(&[b'x'; 23]), Val::Inline { .. }));
        assert!(matches!(Val::store(&[b'x'; 24]), Val::Heap(_)));
        assert_eq!(Val::store(b"").bytes(), Some(&b""[..]), "empty is a value");
        assert_eq!(Val::Absent.bytes(), None, "absent is not");
    }

    #[test]
    fn set_get_clear_across_the_stride() {
        let mut dv = DocValues::new(2);
        dv.set(3, &[Some(b"active"), Some(&[b'y'; 40])]);
        assert_eq!(dv.get(3, 0), Some(&b"active"[..]));
        assert_eq!(dv.get(3, 1), Some(&[b'y'; 40][..]), "long values round-trip");
        assert_eq!(dv.get(3, 2), None, "past the declared arity");
        assert_eq!(dv.get(0, 0), None, "a document that was never set");

        // A short slice leaves the rest absent, and clearing frees the
        // slot for whoever gets the id next.
        dv.set(3, &[Some(b"gone")]);
        assert_eq!(dv.get(3, 1), None, "not carried over from the last write");
        dv.clear(3);
        assert_eq!(dv.get(3, 0), None);
    }

    #[test]
    fn approx_bytes_counts_the_spill() {
        let mut dv = DocValues::new(1);
        dv.set(0, &[Some(b"short")]);
        let inline_only = dv.approx_bytes();
        dv.set(0, &[Some(&[b'z'; 200])]);
        assert!(dv.approx_bytes() > inline_only, "a spilled value costs heap");
    }
}

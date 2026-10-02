//! PFADD, PFCOUNT and PFMERGE.

use super::{
    DENSE, HDR, REGISTERS, SPARSE, cached, dense_set, estimate, histogram, invalidate, merge_into,
    new_sparse, place, raise, to_dense,
};
use crate::{Store, StoreError};

impl Store {
    /// `PFADD key element…`: whether the HyperLogLog was created or any
    /// register rose.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// assert!(s.pfadd(b"h", &[b"a", b"b"]).unwrap());
    /// assert!(!s.pfadd(b"h", &[b"a"]).unwrap());
    /// assert_eq!(s.pfcount(&[b"h"]).unwrap().0, 2);
    /// ```
    pub fn pfadd(&mut self, key: &[u8], elements: &[&[u8]]) -> Result<bool, StoreError> {
        let (mut buf, created) = match self.hll_bytes(key)? {
            Some(b) => (b.into_owned(), false),
            None => (new_sparse(), true),
        };
        let mut raised = false;
        for e in elements {
            let (index, count) = place(e);
            raised |= raise(&mut buf, index, count)?;
        }
        if raised {
            invalidate(&mut buf);
        }
        if raised || created {
            self.set_bytes_keep_ttl(key, buf);
        }
        Ok(raised || created)
    }

    /// `PFCOUNT key…`: the estimated cardinality of the union, and whether
    /// a single key's cached estimate was written — which changes its
    /// bytes, as in Redis. Several keys are counted without caching.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.pfadd(b"h", &[b"a"]).unwrap();
    /// assert_eq!(s.pfcount(&[b"h"]).unwrap(), (1, true));
    /// assert_eq!(s.pfcount(&[b"h"]).unwrap(), (1, false), "read from the cache");
    /// ```
    pub fn pfcount(&mut self, keys: &[&[u8]]) -> Result<(u64, bool), StoreError> {
        if let [key] = keys {
            let Some(b) = self.hll_bytes(key)? else { return Ok((0, false)) };
            if let Some(c) = cached(&b) {
                return Ok((c, false));
            }
            let card = estimate::estimate(&histogram(&b)?);
            let mut buf = b.into_owned();
            buf[8..16].copy_from_slice(&card.to_le_bytes());
            self.set_bytes_keep_ttl(key, buf);
            return Ok((card, true));
        }
        let mut max = alloc::vec![0u8; REGISTERS];
        for key in keys {
            if let Some(b) = self.hll_bytes(key)? {
                merge_into(&mut max, &b)?;
            }
        }
        let mut h = [0u32; 64];
        max.iter().for_each(|&v| h[v as usize] += 1);
        Ok((estimate::estimate(&h), false))
    }

    /// `PFMERGE dst src…`: `dst` becomes the union of itself and the
    /// sources, dense if any of them was.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.pfadd(b"a", &[b"x"]).unwrap();
    /// s.pfadd(b"b", &[b"y"]).unwrap();
    /// s.pfmerge(b"d", &[b"a", b"b"]).unwrap();
    /// assert_eq!(s.pfcount(&[b"d"]).unwrap().0, 2);
    /// ```
    pub fn pfmerge(&mut self, dst: &[u8], srcs: &[&[u8]]) -> Result<(), StoreError> {
        let mut max = alloc::vec![0u8; REGISTERS];
        let mut dense = false;
        for key in core::iter::once(&dst).chain(srcs) {
            if let Some(b) = self.hll_bytes(key)? {
                dense |= b[4] == DENSE;
                merge_into(&mut max, &b)?;
            }
        }
        let mut buf = match self.hll_bytes(dst)? {
            Some(b) => b.into_owned(),
            None => new_sparse(),
        };
        if dense && buf[4] == SPARSE {
            buf = to_dense(&buf)?;
        }
        if dense {
            for (i, &v) in max.iter().enumerate() {
                dense_set(&mut buf[HDR..], i, v);
            }
        } else {
            // register by register, so the sparse bytes come out as Redis's
            for (i, &v) in max.iter().enumerate().filter(|(_, v)| **v != 0) {
                raise(&mut buf, i as u32, v)?;
            }
        }
        invalidate(&mut buf);
        self.set_bytes_keep_ttl(dst, buf);
        Ok(())
    }
}

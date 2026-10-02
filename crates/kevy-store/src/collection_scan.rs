//! `HSCAN` / `SSCAN` / `ZSCAN` pages over one collection.
//!
//! A small inline collection comes back whole with cursor `0`, as Redis
//! answers its listpack and intset encodings. A flat table is walked with
//! [`kevy_map::KevyMap::scan_step`], a sharded one with
//! [`crate::seg_map::SegMap::scan_page`]; each takes the cursor the other
//! hands out as a fresh start, so a collection promoted between pages is
//! swept again from the beginning — members may repeat, none present
//! throughout is missed, the contract Redis's cursors keep.

use kevy_hash::KevyHash;
use kevy_map::KevyMap;

use crate::seg_map::SEG_CURSOR;
use crate::{Store, StoreError, Value};

/// A flat table's pages: whole home groups until `count` entries are out.
fn flat_page<K: KevyHash + Eq, V>(
    m: &KevyMap<K, V>,
    cursor: u64,
    count: usize,
    mut f: impl FnMut(&K, &V),
) -> u64 {
    let mut c = if cursor & SEG_CURSOR != 0 { 0 } else { cursor };
    let mut emitted = 0;
    loop {
        c = m.scan_step(c, |k, v| {
            f(k, v);
            emitted += 1;
        });
        if c == 0 || emitted >= count {
            return c;
        }
    }
}

impl Store {
    /// One `HSCAN` page of `key`: each field and value to `f`, and the
    /// cursor to continue from (`0` ends the sweep). A missing key is an
    /// empty sweep.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.hset(b"h", &[(b"f".as_slice(), b"v".as_slice())])?;
    /// let mut got = Vec::new();
    /// let next = s.hscan(b"h", 0, 10, |f, v| got.push((f.to_vec(), v.to_vec())))?;
    /// assert_eq!((next, got), (0, vec![(b"f".to_vec(), b"v".to_vec())]));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn hscan(
        &mut self,
        key: &[u8],
        cursor: u64,
        count: usize,
        mut f: impl FnMut(&[u8], &[u8]),
    ) -> Result<u64, StoreError> {
        self.purge_hash_ttl(key);
        let Some(e) = self.tier_serve(key, crate::value::COLD_TAG_HASH)? else { return Ok(0) };
        Ok(match &e.value {
            Value::Hash(h) => flat_page(h, cursor, count, |k, v| f(k.as_slice(), v.as_slice())),
            Value::SegHash(h) => h.scan_page(cursor, count, |k, v| f(k, v.as_slice())),
            Value::SmallHashInline(h) => {
                h.iter().for_each(|(k, v)| f(k, v));
                0
            }
            Value::PackedRow(r) => {
                r.fields().for_each(|(k, v)| f(k, v));
                0
            }
            _ => return Err(StoreError::WrongType),
        })
    }

    /// One `SSCAN` page of `key`, each member to `f`.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.sadd(b"s", &[b"a"])?;
    /// let mut got = Vec::new();
    /// assert_eq!(s.sscan(b"s", 0, 10, |m| got.push(m.to_vec()))?, 0);
    /// assert_eq!(got, vec![b"a".to_vec()]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn sscan(
        &mut self,
        key: &[u8],
        cursor: u64,
        count: usize,
        mut f: impl FnMut(&[u8]),
    ) -> Result<u64, StoreError> {
        let Some(e) = self.live_entry(key) else { return Ok(0) };
        Ok(match &e.value {
            Value::Set(s) => {
                let mut c = if cursor & SEG_CURSOR != 0 { 0 } else { cursor };
                let mut emitted = 0;
                loop {
                    c = s.scan_step(c, |m| {
                        f(m.as_slice());
                        emitted += 1;
                    });
                    if c == 0 || emitted >= count {
                        break c;
                    }
                }
            }
            Value::SegSet(s) => s.scan_page(cursor, count, |m, ()| f(m)),
            Value::SmallSetInline(s) => {
                s.iter().for_each(&mut f);
                0
            }
            _ => return Err(StoreError::WrongType),
        })
    }

    /// One `ZSCAN` page of `key`, each member and its score to `f`.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.zadd(b"z", &[(1.5, b"a".as_slice())])?;
    /// let mut got = Vec::new();
    /// assert_eq!(s.zscan(b"z", 0, 10, |m, sc| got.push((m.to_vec(), sc)))?, 0);
    /// assert_eq!(got, vec![(b"a".to_vec(), 1.5)]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn zscan(
        &mut self,
        key: &[u8],
        cursor: u64,
        count: usize,
        mut f: impl FnMut(&[u8], f64),
    ) -> Result<u64, StoreError> {
        let Some(e) = self.live_entry(key) else { return Ok(0) };
        Ok(match &e.value {
            Value::ZSet(z) => flat_page(&z.by_member, cursor, count, |m, sc| f(m.as_slice(), *sc)),
            Value::SegZSet(z) => z.by_member().scan_page(cursor, count, |m, sc| f(m, *sc)),
            Value::SmallZSetInline(z) => {
                z.iter().for_each(|(m, sc)| f(m, sc));
                0
            }
            _ => return Err(StoreError::WrongType),
        })
    }
}

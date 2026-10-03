//! `LPOP` / `RPOP`: elements handed out as they go, an inline list
//! popped in its own buffer.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use alloc::borrow::Cow;

use crate::list::flat_delta;
use crate::value::{Value, list_item_weight};
use crate::{Store, StoreError};

impl Store {
    /// `LPOP` / `RPOP` (`front` picks which) of up to `count`
    /// elements, each handed to `f` as it goes — borrowed from an inline
    /// list, owned from a heap one — deleting the key once emptied. How
    /// many went.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.rpush(b"l", &[b"a".as_slice(), b"b", b"c"])?;
    /// let mut got = Vec::new();
    /// assert_eq!(s.list_pop_each(b"l", 2, false, |e| got.push(e.into_owned()))?, 2);
    /// assert_eq!(got, [b"c".to_vec(), b"b".to_vec()]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn list_pop_each(
        &mut self,
        key: &[u8],
        count: usize,
        front: bool,
        mut f: impl FnMut(Cow<'_, [u8]>),
    ) -> Result<usize, StoreError> {
        if let Some(Value::SmallListInline(l)) = self.live_entry_mut(key).map(|e| &mut e.value) {
            let mut n = 0;
            while n < count && l.pop_with(front, |e| f(Cow::Borrowed(e))) {
                n += 1;
            }
            self.drop_if_empty_list(key);
            return Ok(n);
        }
        let mut n = 0;
        let mut delta: i64 = 0;
        if self.is_seglist(key) {
            let l = self.seglist_mut(key);
            while n < count {
                let Some(v) = (if front { l.pop_front() } else { l.pop_back() }) else { break };
                delta -= list_item_weight(v.len()) as i64;
                f(Cow::Owned(v));
                n += 1;
            }
        } else if let Some(l) = self.list_mut(key, false)? {
            let cap = l.capacity();
            while n < count {
                let Some(v) = (if front { l.pop_front() } else { l.pop_back() }) else { break };
                delta -= v.capacity() as i64;
                f(Cow::Owned(v));
                n += 1;
            }
            delta = flat_delta(l, cap, delta);
        }
        self.account_delta(key, delta);
        self.drop_if_empty_list(key);
        Ok(n)
    }

    fn list_pop(
        &mut self,
        key: &[u8],
        count: usize,
        front: bool,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        let mut out = Vec::new();
        self.list_pop_each(key, count, front, |e| out.push(e.into_owned()))?;
        Ok(out)
    }

    /// `LPOP` — pop up to `count` from the head (deleting emptied key).
    pub fn lpop(&mut self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>, StoreError> {
        self.list_pop(key, count, true)
    }

    /// `RPOP` — pop up to `count` from the tail.
    pub fn rpop(&mut self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>, StoreError> {
        self.list_pop(key, count, false)
    }
}

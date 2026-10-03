//! A collection's elements borrowed in their own order, for a reader that
//! needs all of them at once and copies none.

use crate::value::Value;
use crate::{Store, StoreError};

impl Store {
    /// Each element of the list, set or sorted set at `key`, in its own
    /// order — a list's, a set's iteration, a sorted set's by score — to
    /// `f`, borrowed for as long as the store is; then the key's type
    /// name, `"none"` for a missing key. Any other type is an error.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.zadd(b"z", &[(2.0, b"b".as_slice()), (1.0, b"a")])?;
    /// let mut seen: Vec<&[u8]> = Vec::new();
    /// assert_eq!(s.each_element(b"z", |m| seen.push(m))?, "zset");
    /// assert_eq!(seen, [&b"a"[..], b"b"]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn each_element<'s>(
        &'s mut self,
        key: &[u8],
        mut f: impl FnMut(&'s [u8]),
    ) -> Result<&'static str, StoreError> {
        let Some(e) = self.live_entry(key) else { return Ok("none") };
        match &e.value {
            Value::SmallListInline(l) => l.iter().for_each(&mut f),
            Value::List(l) => l.iter().for_each(|v| f(v)),
            Value::SegList(l) => l.iter().for_each(|v| f(v)),
            Value::SmallSetInline(s) => s.iter().for_each(&mut f),
            Value::Set(s) => s.iter().for_each(|m| f(m.as_slice())),
            Value::SegSet(s) => s.keys().for_each(|m| f(m.as_slice())),
            Value::ZSet(z) => z.ordered().for_each(|(m, _)| f(m)),
            Value::SegZSet(z) => z.ordered().for_each(|(m, _)| f(m)),
            Value::SmallZSetInline(z) => {
                let mut two = [(&[][..], 0.0); 2];
                let n = z.iter().zip(two.iter_mut()).map(|(x, slot)| *slot = x).count();
                two[..n].sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(b.0)));
                two[..n].iter().for_each(|&(m, _)| f(m));
            }
            _ => return Err(StoreError::WrongType),
        }
        Ok(e.value.type_name())
    }
}

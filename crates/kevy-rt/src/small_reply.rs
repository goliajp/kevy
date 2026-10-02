//! [`SmallReply`]'s constructors and views; the type itself lives with
//! the other message types in [`crate::message`].

use crate::message::SmallReply;

impl SmallReply {
    /// Copy `b` into the inline arm when it fits, else one heap alloc.
    #[inline]
    pub(crate) fn from_slice(b: &[u8]) -> Self {
        if b.len() <= 30 {
            let mut buf = [0u8; 30];
            buf[..b.len()].copy_from_slice(b);
            SmallReply::Inline { len: b.len() as u8, buf }
        } else {
            SmallReply::Heap(b.to_vec())
        }
    }

    /// Wrap an already-owned `Vec` — zero-copy for the heap arm.
    #[inline]
    pub(crate) fn from_vec(v: Vec<u8>) -> Self {
        SmallReply::Heap(v)
    }

    /// The same reply with its RESP2 nulls swapped for RESP3's `_`. Not
    /// for a parked reply: those come from a dispatch, which already
    /// answered in the conn's protocol.
    pub(crate) fn resp3_nulls(self) -> Self {
        match self {
            SmallReply::Inline { mut len, mut buf } => {
                len = kevy_resp::resp3_nulls(&mut buf[..len as usize]) as u8;
                SmallReply::Inline { len, buf }
            }
            SmallReply::Heap(mut v) => {
                let n = kevy_resp::resp3_nulls(&mut v);
                v.truncate(n);
                SmallReply::Heap(v)
            }
            parked => parked,
        }
    }

    /// The reply's bytes; `parked` is the owning conn's parked buffer.
    #[inline]
    pub(crate) fn bytes<'a>(&'a self, parked: &'a [u8]) -> &'a [u8] {
        match self {
            SmallReply::Inline { len, buf } => &buf[..*len as usize],
            SmallReply::Heap(v) => v,
            SmallReply::Parked { off, len } => &parked[*off as usize..(*off + *len) as usize],
        }
    }
}

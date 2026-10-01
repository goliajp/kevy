//! The `{hashtag}` of a key, Redis Cluster's rule: the bytes between the
//! first `{` and the first `}` after it, when there are any.
//!
//! Every key that is routed asks this, and almost none has a tag, so the
//! whole key is scanned for a `{` that is not there. The scan reads eight
//! bytes a step.

const LO: u64 = 0x0101_0101_0101_0101;
const HI: u64 = 0x8080_8080_8080_8080;

/// Where in `word` (eight bytes, read little-endian) the first `b` is.
#[inline(always)]
fn in_word(word: &[u8], pat: u64) -> Option<usize> {
    let x = u64::from_le_bytes(word.try_into().expect("eight bytes")) ^ pat;
    // the lowest byte flagged is the first zero byte of x exactly; a
    // borrow can flag only bytes above it
    let m = x.wrapping_sub(LO) & !x & HI;
    (m != 0).then_some((m.trailing_zeros() / 8) as usize)
}

/// The first `b` in `bytes`.
#[inline]
fn find(bytes: &[u8], b: u8) -> Option<usize> {
    let pat = LO * u64::from(b);
    let n = bytes.len();
    if n < 8 {
        return bytes.iter().position(|&c| c == b);
    }
    let mut i = 0;
    while i + 8 <= n {
        if let Some(p) = in_word(&bytes[i..i + 8], pat) {
            return Some(i + p);
        }
        i += 8;
    }
    // the tail as the last eight bytes, whose front was already searched
    if i < n {
        return in_word(&bytes[n - 8..], pat).map(|p| n - 8 + p);
    }
    None
}

/// The `{hashtag}` of `key`, or `None` when it has none: no `{`, no `}`
/// after it, or nothing between the two.
///
/// ```
/// assert_eq!(kevy_hash::hashtag(b"{user:1}:cart"), Some(&b"user:1"[..]));
/// assert_eq!(kevy_hash::hashtag(b"a{}{b}"), None);
/// assert_eq!(kevy_hash::hashtag(b"plain:key"), None);
/// ```
#[inline]
pub fn hashtag(key: &[u8]) -> Option<&[u8]> {
    let start = find(key, b'{')?;
    let after = &key[start + 1..];
    match find(after, b'}')? {
        0 => None,
        len => Some(&after[..len]),
    }
}

#[cfg(test)]
mod tests {
    use super::{find, hashtag};

    fn reference(key: &[u8]) -> Option<&[u8]> {
        let start = key.iter().position(|&b| b == b'{')?;
        let after = &key[start + 1..];
        let len = after.iter().position(|&b| b == b'}')?;
        (len > 0).then(|| &after[..len])
    }

    #[test]
    fn finds_the_first_byte_at_every_offset_among_lookalikes() {
        // 0xfb is `{` with the high bit set and 0x7a / 0x7c sit beside it:
        // the bytes a word-at-a-time compare could mistake for it
        for len in 0..40 {
            for at in 0..=len {
                let mut v: Vec<u8> =
                    (0..len).map(|i| [0xfb, 0x7a, 0x7c, 0x00, 0xff][i % 5]).collect();
                if at < len {
                    v[at] = b'{';
                    if at + 3 < len {
                        v[at + 3] = b'{';
                    }
                }
                let want = v.iter().position(|&b| b == b'{');
                assert_eq!(find(&v, b'{'), want, "len {len} at {at}");
            }
        }
    }

    #[test]
    fn agrees_with_the_bytewise_rule() {
        let shapes: [&[u8]; 9] =
            [b"", b"{", b"}", b"{}", b"{a}", b"x{}{b}", b"k:000000012345", b"{u1}:a", b"a{b{c}d}"];
        for s in shapes {
            assert_eq!(hashtag(s), reference(s), "{:?}", String::from_utf8_lossy(s));
        }
        for len in 0..24 {
            for open in 0..len {
                for close in 0..len {
                    let mut v = vec![b'x'; len];
                    v[open] = b'{';
                    v[close] = b'}';
                    assert_eq!(hashtag(&v), reference(&v), "{v:?}");
                }
            }
        }
    }
}

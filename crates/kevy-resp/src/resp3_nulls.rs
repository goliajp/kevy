//! RESP3 has one null, `_`; RESP2 has two, the null bulk `$-1` and the
//! null array `*-1`. A reply built by a RESP2 encoder is turned into its
//! RESP3 form by swapping those two headers, wherever they sit.

/// Rewrite, in place, every RESP2 null in the complete replies in `buf`
/// to `_\r\n`, nested ones included, and return the new length (the
/// bytes past it are left over). Walks frame headers only — a bulk's
/// payload is stepped over by its length — so the cost follows the number
/// of frames, not of bytes, and a reply without a null is not written.
///
/// ```
/// let mut out = b"+OK\r\n".to_vec();
/// kevy_resp::encode_array_len(&mut out, 3);
/// kevy_resp::encode_bulk(&mut out, b"$-1\r\n");
/// kevy_resp::encode_null_bulk(&mut out);
/// out.extend_from_slice(b"*-1\r\n");
/// let n = kevy_resp::resp3_nulls(&mut out[5..]);
/// out.truncate(5 + n);
/// assert_eq!(out, b"+OK\r\n*3\r\n$5\r\n$-1\r\n\r\n_\r\n_\r\n");
/// ```
pub fn resp3_nulls(out: &mut [u8]) -> usize {
    let Some(first) = next_null(out, 0) else { return out.len() };
    let (mut r, mut w) = (first, first);
    while r < out.len() {
        let (frame, null) = frame_at(out, r);
        if null {
            out[w..w + 3].copy_from_slice(b"_\r\n");
            w += 3;
        } else {
            out.copy_within(r..r + frame, w);
            w += frame;
        }
        r += frame;
    }
    w
}

/// Where the first null header at or after `at` starts.
fn next_null(out: &[u8], mut at: usize) -> Option<usize> {
    while at < out.len() {
        let (frame, null) = frame_at(out, at);
        if null {
            return Some(at);
        }
        at += frame;
    }
    None
}

/// Length of the header (plus payload, for a bulk-shaped frame) at `at`,
/// and whether it is a RESP2 null.
fn frame_at(out: &[u8], at: usize) -> (usize, bool) {
    let mut eol = at + 1;
    while out[eol] != b'\r' {
        eol += 1;
    }
    let line = &out[at + 1..eol];
    let header = eol + 2 - at;
    match out[at] {
        b'$' | b'*' if line == b"-1" => (header, true),
        b'$' | b'=' | b'!' => (header + parse_len(line) + 2, false),
        _ => (header, false),
    }
}

fn parse_len(digits: &[u8]) -> usize {
    digits.iter().fold(0, |n, d| n * 10 + usize::from(d - b'0'))
}

#[cfg(test)]
mod tests {
    use super::resp3_nulls;

    fn conv(v: &[u8]) -> Vec<u8> {
        let mut out = v.to_vec();
        let n = resp3_nulls(&mut out);
        out.truncate(n);
        out
    }

    #[test]
    fn every_null_shape_becomes_underscore() {
        assert_eq!(conv(b"$-1\r\n"), b"_\r\n");
        assert_eq!(conv(b"*-1\r\n"), b"_\r\n");
        assert_eq!(conv(b"*3\r\n$1\r\nv\r\n$-1\r\n*-1\r\n"), b"*3\r\n$1\r\nv\r\n_\r\n_\r\n");
        assert_eq!(conv(b"*1\r\n*2\r\n$-1\r\n:5\r\n"), b"*1\r\n*2\r\n_\r\n:5\r\n");
        assert_eq!(
            conv(b">3\r\n$5\r\nhello\r\n$-1\r\n:0\r\n"),
            b">3\r\n$5\r\nhello\r\n_\r\n:0\r\n"
        );
    }

    #[test]
    fn payloads_that_look_like_nulls_are_data() {
        let same: [&[u8]; 5] = [
            b"$5\r\n$-1\r\n\r\n",
            b"$7\r\n\r\n$-1\r\n\r\n",
            b"=9\r\ntxt:$-1\r\n\r\n",
            b"-ERR $-1\r\n",
            b"*2\r\n:-1\r\n,-1\r\n",
        ];
        for v in same {
            assert_eq!(conv(v), v, "{:?}", core::str::from_utf8(v));
        }
        assert_eq!(conv(b"$5\r\n$-1\r\n\r\n$-1\r\n"), b"$5\r\n$-1\r\n\r\n_\r\n");
    }

    #[test]
    fn a_sub_slice_is_rewritten_alone() {
        let mut out = b"$-1\r\n$-1\r\n".to_vec();
        let n = resp3_nulls(&mut out[5..]);
        out.truncate(5 + n);
        assert_eq!(out, b"$-1\r\n_\r\n");
    }
}

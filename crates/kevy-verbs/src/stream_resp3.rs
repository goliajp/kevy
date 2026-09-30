//! The RESP3 shape of the stream replies that change under `HELLO 3`:
//! a missing value is the null type instead of a RESP2 null bulk or null
//! array, and `XREAD` / `XREADGROUP` answer a map of key to entries
//! instead of an array of `[key, entries]` pairs. The commands write
//! their RESP2 reply; a RESP3 caller rewrites it with [`stream_resp3`].

use kevy_resp::{encode_map_header, encode_null};

/// Rewrite the RESP2 reply `verb` wrote at `out[from..]` into its RESP3
/// shape. A verb whose reply does not change is left as it is.
///
/// ```
/// let mut out = b"*1\r\n*2\r\n$1\r\ns\r\n*1\r\n*2\r\n$3\r\n1-1\r\n*-1\r\n".to_vec();
/// kevy_verbs::cmd::stream_resp3(b"XREADGROUP", &mut out, 0);
/// assert_eq!(out, b"%1\r\n$1\r\ns\r\n*1\r\n*2\r\n$3\r\n1-1\r\n_\r\n");
/// ```
pub fn stream_resp3(verb: &[u8], out: &mut Vec<u8>, from: usize) {
    let map = match verb {
        b"XREAD" | b"XREADGROUP" => true,
        b"XRANGE" | b"XREVRANGE" | b"XADD" | b"XPENDING" | b"XAUTOCLAIM" | b"XCLAIM" => false,
        _ => return,
    };
    if out.get(from) == Some(&b'-') {
        return;
    }
    let reply = out.split_off(from);
    let mut at = 0;
    if map && reply.starts_with(b"*") && !reply.starts_with(b"*-1") {
        let (n, body) = header(&reply, 0);
        encode_map_header(out, n);
        at = body;
        for _ in 0..n {
            // each pair `*2 key entries` becomes `key entries`
            let (_, inner) = header(&reply, at);
            at = inner;
            at = copy_nulls(&reply, at, out);
            at = copy_nulls(&reply, at, out);
        }
        return;
    }
    while at < reply.len() {
        at = copy_nulls(&reply, at, out);
    }
}

/// Copy one RESP2 value at `buf[at..]` to `out`, its nulls as the RESP3
/// null; the offset after it.
fn copy_nulls(buf: &[u8], at: usize, out: &mut Vec<u8>) -> usize {
    let t = buf[at];
    let (n, body) = header(buf, at);
    match t {
        b'$' | b'*' if n < 0 => {
            encode_null(out);
            body
        }
        b'$' => {
            let end = body + n as usize + 2;
            out.extend_from_slice(&buf[at..end]);
            end
        }
        b'*' => {
            out.extend_from_slice(&buf[at..body]);
            (0..n).fold(body, |i, _| copy_nulls(buf, i, out))
        }
        _ => {
            out.extend_from_slice(&buf[at..body]);
            body
        }
    }
}

/// The number on the line at `buf[at..]` (0 for a line without one) and
/// the offset past the line.
fn header(buf: &[u8], at: usize) -> (i64, usize) {
    let end = at + buf[at..].windows(2).position(|w| w == b"\r\n").unwrap_or(buf.len() - at);
    let n = std::str::from_utf8(&buf[at + 1..end]).ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    (n, end + 2)
}

#[cfg(test)]
mod tests {
    use super::stream_resp3;

    fn v3(verb: &[u8], resp2: &[u8]) -> Vec<u8> {
        let mut out = b"+kept\r\n".to_vec();
        out.extend_from_slice(resp2);
        stream_resp3(verb, &mut out, 7);
        out[7..].to_vec()
    }

    #[test]
    fn nulls_become_the_null_type() {
        assert_eq!(v3(b"XRANGE", b"*-1\r\n"), b"_\r\n");
        assert_eq!(v3(b"XADD", b"$-1\r\n"), b"_\r\n");
        assert_eq!(
            v3(b"XPENDING", b"*4\r\n:0\r\n$-1\r\n$-1\r\n*-1\r\n"),
            b"*4\r\n:0\r\n_\r\n_\r\n_\r\n"
        );
        assert_eq!(v3(b"XREAD", b"*-1\r\n"), b"_\r\n");
        assert_eq!(v3(b"XLEN", b":1\r\n"), b":1\r\n", "not a verb that changes");
        assert_eq!(v3(b"XRANGE", b"-ERR x\r\n"), b"-ERR x\r\n");
    }

    #[test]
    fn a_read_answers_a_map_of_key_to_entries() {
        let resp2 = b"*2\r\n*2\r\n$1\r\na\r\n*0\r\n*2\r\n$1\r\nb\r\n*1\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n";
        let want = b"%2\r\n$1\r\na\r\n*0\r\n$1\r\nb\r\n*1\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n";
        assert_eq!(v3(b"XREADGROUP", resp2), want);
    }
}

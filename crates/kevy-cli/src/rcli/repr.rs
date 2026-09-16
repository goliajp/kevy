//! How redis-cli quotes a string for a human.
//!
//! Printable ASCII as itself, the C escapes by name, and every other byte —
//! 0x80 and up included — as `\xHH`, which is what redis-cli shows.

/// Append the quoted representation of `bytes` to `out`.
pub(crate) fn push_repr(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'"');
    for &b in bytes {
        match b {
            b'\\' | b'"' => out.extend_from_slice(&[b'\\', b]),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x07 => out.extend_from_slice(b"\\a"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x20..=0x7e => out.push(b),
            _ => out.extend_from_slice(format!("\\x{b:02x}").as_bytes()),
        }
    }
    out.push(b'"');
}

/// The quoted representation of `bytes` as a new buffer.
pub(crate) fn repr(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 2);
    push_repr(&mut out, bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::repr;

    #[test]
    fn every_byte_class() {
        assert_eq!(repr(b"a\"\\\n\r\t\x07\x08"), br#""a\"\\\n\r\t\a\b""#.to_vec());
        assert_eq!(repr(b"\x00\x1f\x7f\x80\xff ~"), br#""\x00\x1f\x7f\x80\xff ~""#.to_vec());
        assert_eq!(repr(b""), b"\"\"".to_vec());
    }
}

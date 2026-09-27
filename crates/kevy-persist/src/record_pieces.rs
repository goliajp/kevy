//! The append path's record encoder: a command's RESP multibulk frame is
//! written into its sink piece by piece (length headers from a stack
//! buffer, argument bytes straight from the caller's slices), and the v2
//! envelope's length and CRC32C are computed over the same pieces. The
//! bytes are exactly what [`crate::write_multibulk`] and
//! `record::write_record` produce; the frame is just never joined into
//! an intermediate buffer first.

use std::convert::Infallible;
use std::io::{self, Write};

use kevy_resp::ArgvView;

use crate::crc32c::crc32c_append;
use crate::record::RECORD_HEADER;

/// Longest run of framing bytes between two argument bodies:
/// `\r\n` + `*<argc>\r\n` + `$<len>\r\n`, each count up to 20 digits.
const RUN: usize = 2 + 2 * (1 + 20 + 2);

/// Feed `f` the frame of `args` in order; concatenated, the pieces are
/// the multibulk encoding.
fn for_each_piece<A, E, F>(args: &A, mut f: F) -> Result<(), E>
where
    A: ArgvView + ?Sized,
    F: FnMut(&[u8]) -> Result<(), E>,
{
    let mut run = [0u8; RUN];
    let mut n = put_count(&mut run, 0, b'*', args.len());
    for i in 0..args.len() {
        let a = &args[i];
        n = put_count(&mut run, n, b'$', a.len());
        f(&run[..n])?;
        f(a)?;
        run[..2].copy_from_slice(b"\r\n");
        n = 2;
    }
    f(&run[..n])
}

/// Write `<tag><decimal count>\r\n` at `run[at..]`, returning the new end.
fn put_count(run: &mut [u8; RUN], at: usize, tag: u8, count: usize) -> usize {
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    let mut x = count;
    loop {
        i -= 1;
        digits[i] = b'0' + (x % 10) as u8;
        x /= 10;
        if x == 0 {
            break;
        }
    }
    let d = &digits[i..];
    run[at] = tag;
    run[at + 1..at + 1 + d.len()].copy_from_slice(d);
    let end = at + 1 + d.len();
    run[end..end + 2].copy_from_slice(b"\r\n");
    end + 2
}

/// The v2 envelope header (`u32-LE len`, `u32-LE crc32c`) for the record
/// wrapping `args`'s multibulk frame.
pub(crate) fn record_header<A: ArgvView + ?Sized>(args: &A) -> [u8; RECORD_HEADER] {
    let (mut len, mut crc) = (0usize, 0u32);
    let Ok(()) = for_each_piece(args, |p| -> Result<(), Infallible> {
        len += p.len();
        crc = crc32c_append(crc, p);
        Ok(())
    });
    let mut h = [0u8; RECORD_HEADER];
    h[..4].copy_from_slice(&(len as u32).to_le_bytes());
    h[4..].copy_from_slice(&crc.to_le_bytes());
    h
}

/// Write one record: the envelope header when given (v2), then the frame.
pub(crate) fn write_frame<W, A>(
    w: &mut W,
    head: Option<&[u8; RECORD_HEADER]>,
    args: &A,
) -> io::Result<()>
where
    W: Write + ?Sized,
    A: ArgvView + ?Sized,
{
    if let Some(h) = head {
        w.write_all(h)?;
    }
    for_each_piece(args, |p| w.write_all(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kevy_resp::Argv;

    fn argv(parts: &[&[u8]]) -> Argv {
        let mut a = Argv::default();
        for p in parts {
            a.push(p);
        }
        a
    }

    // the pieces join to the reference encodings, bare and enveloped
    #[test]
    fn pieces_match_reference_encoders() {
        let big = vec![0xA5u8; 300_000];
        let cases: Vec<Argv> = vec![
            argv(&[]),
            argv(&[b"PING"]),
            argv(&[b"SET", b"k", b""]),
            argv(&[b"SET", b"k", b"v"]),
            argv(&[b"SET", b"key", &big]),
            argv(&[b"HSET", b"h", b"f1", b"v1", b"f2", b"0123456789"]),
        ];
        for a in &cases {
            let mut want = Vec::new();
            crate::write_multibulk(&mut want, a).unwrap();
            let mut got = Vec::new();
            write_frame(&mut got, None, a).unwrap();
            assert_eq!(got, want, "bare frame of {} args", a.len());

            let mut want = Vec::new();
            crate::record::write_record_multibulk(&mut want, a, &mut Vec::new()).unwrap();
            let mut got = Vec::new();
            write_frame(&mut got, Some(&record_header(a)), a).unwrap();
            assert_eq!(got, want, "record of {} args", a.len());
        }
    }

    // pinned against bytes computed outside this crate, so a change shared
    // by the encoder and the reference cannot pass unnoticed
    #[test]
    fn set_record_matches_literal_bytes() {
        let mut got = Vec::new();
        let a = argv(&[b"SET", b"k", b"v"]);
        write_frame(&mut got, Some(&record_header(&a)), &a).unwrap();
        let mut want = vec![27, 0, 0, 0, 0x6b, 0x95, 0x66, 0x64];
        want.extend_from_slice(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n");
        assert_eq!(got, want);
    }
}

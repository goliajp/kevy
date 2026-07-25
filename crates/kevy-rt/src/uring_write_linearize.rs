//! Short-write recovery for the chunked-writev output path.
//!
//! The writev path interleaves `write_buf` header bytes with borrowed
//! `Arc<Box<[u8]>>` value bodies (zero-copy GET replies, pub/sub
//! fan-out). A burst can exceed `IOV_MAX`, so the arm loop submits the
//! payload in chunks and `write_off` walks forward through `write_buf`
//! as each chunk completes.
//!
//! When a chunk comes back SHORT, the remaining payload can no longer be
//! described by "arcs + an offset" — the kernel stopped somewhere inside
//! the interleaving. This module flattens everything still unsent into
//! one linear buffer so the next iteration resumes on the plain
//! `prep_write` path.
//!
//! The subtle part, and the reason this is a separate, directly tested
//! unit: the flatten must start at `write_off`, NOT at zero. Bytes
//! before `write_off` are already on the wire. Re-including them
//! re-transmits them, and a duplicated prefix does not read as "some
//! extra bytes" to the peer — it desynchronises the RESP framing, after
//! which every reply the client is waiting for fails to parse and the
//! connection looks wedged with the request dispatched and the response
//! "missing".

use std::sync::Arc;

/// Flatten every still-unsent byte — the in-flight chunk's unsent
/// suffix, the arcs the chunk didn't reach, and the `write_buf` tail —
/// into one linear buffer.
///
/// `write_off` is where the just-completed chunk STARTED; `written` is
/// how many bytes of it the kernel actually took. Returns the new
/// `(write_buf, write_off)` pair: the buffer begins at the old
/// `write_off`, so the new offset is simply `written`.
///
/// `arcs` must be sorted by position and every position must be at or
/// after `write_off` (the arm loop's iovec builder walks forward from
/// `write_off` and caps on an arc boundary, so it cannot leave an arc
/// behind it).
pub(crate) fn linearize_unsent(
    write_buf: &[u8],
    arcs: &[(usize, Arc<Box<[u8]>>)],
    write_off: usize,
    written: usize,
) -> (Vec<u8>, usize) {
    let tail = write_buf.len().saturating_sub(write_off);
    let total = tail + arcs.iter().map(|(_, a)| a.len()).sum::<usize>();
    let mut linear = Vec::with_capacity(total);
    let mut prev = write_off;
    for (pos, arc) in arcs {
        let pos = (*pos).max(prev);
        if pos > prev {
            linear.extend_from_slice(&write_buf[prev..pos]);
        }
        linear.extend_from_slice(arc.as_ref());
        prev = pos;
    }
    if prev < write_buf.len() {
        linear.extend_from_slice(&write_buf[prev..]);
    }
    (linear, written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arc(b: &[u8]) -> Arc<Box<[u8]>> {
        Arc::new(b.to_vec().into_boxed_slice())
    }

    /// What the wire has seen so far, plus what `linearize_unsent`
    /// leaves to send, must equal the payload exactly once.
    fn wire_after(
        buf: &[u8],
        arcs: &[(usize, Arc<Box<[u8]>>)],
        write_off: usize,
        written: usize,
    ) -> Vec<u8> {
        // Bytes already on the wire before this chunk: the interleaving
        // from 0 up to write_off (headers only — every arc at a position
        // < write_off was consumed by an earlier chunk and drained).
        let mut sent = buf[..write_off].to_vec();
        let (rest, off) = linearize_unsent(buf, arcs, write_off, written);
        sent.extend_from_slice(&rest[..off]); // this chunk's accepted part
        sent.extend_from_slice(&rest[off..]); // what we will send next
        sent
    }

    #[test]
    fn first_chunk_short_write_flattens_the_interleaving() {
        // No prior chunk: write_off = 0, so old and new behaviour agree.
        let buf = b"HDR1|HDR2|TAIL".to_vec();
        let arcs = vec![(5, arc(b"AAAA")), (10, arc(b"BBBB"))];
        let (linear, off) = linearize_unsent(&buf, &arcs, 0, 6);
        assert_eq!(linear, b"HDR1|AAAAHDR2|BBBBTAIL".to_vec());
        assert_eq!(off, 6);
    }

    /// The regression this module exists for. A capped chunk completed
    /// fully (write_off advanced to the cap), the NEXT chunk came back
    /// short — flattening from zero would put the already-transmitted
    /// prefix back into the buffer while the offset only accounts for
    /// this chunk, re-sending it and desynchronising RESP framing.
    #[test]
    fn resumed_chunk_short_write_does_not_resend_the_prefix() {
        let buf = b"HDR1|HDR2|TAIL".to_vec();
        // Arc at position 5 was consumed by the first chunk and drained;
        // write_off advanced to 10, the cap. Only the second arc remains.
        let arcs = vec![(10, arc(b"BBBB"))];
        let (linear, off) = linearize_unsent(&buf, &arcs, 10, 2);

        // Starts at write_off — the first 10 bytes are already gone.
        assert_eq!(linear, b"BBBBTAIL".to_vec());
        assert_eq!(off, 2);
        // And nothing before write_off leaked back in.
        assert!(!linear.starts_with(b"HDR1"));

        // The wire sees each byte exactly once.
        assert_eq!(wire_after(&buf, &arcs, 10, 2), b"HDR1|HDR2|BBBBTAIL".to_vec());
    }

    #[test]
    fn short_write_landing_inside_an_arc_keeps_the_remainder() {
        let buf = b"HDR|".to_vec();
        let arcs = vec![(4, arc(b"VALUEVALUE"))];
        // 6 bytes accepted: "HDR|" + "VA".
        let (linear, off) = linearize_unsent(&buf, &arcs, 0, 6);
        assert_eq!(linear, b"HDR|VALUEVALUE".to_vec());
        assert_eq!(&linear[off..], b"LUEVALUE");
    }

    #[test]
    fn zero_accepted_bytes_resends_nothing_twice() {
        let buf = b"HDR1|HDR2|".to_vec();
        let arcs = vec![(10, arc(b"BB"))];
        let (linear, off) = linearize_unsent(&buf, &arcs, 5, 0);
        assert_eq!(off, 0);
        assert_eq!(linear, b"HDR2|BB".to_vec());
        assert_eq!(wire_after(&buf, &arcs, 5, 0), b"HDR1|HDR2|BB".to_vec());
    }

    #[test]
    fn no_arcs_left_is_just_the_tail() {
        let buf = b"HDR1|HDR2|TAIL".to_vec();
        let (linear, off) = linearize_unsent(&buf, &[], 10, 3);
        assert_eq!(linear, b"TAIL".to_vec());
        assert_eq!(off, 3);
    }
}

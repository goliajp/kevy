//! A dictionary parsed once, and the decode that uses it.
//!
//! Split from `lib.rs` so the assembly point stays one — and because
//! this is one subject: the dictionary is per-FILE state, and every
//! entry point in this crate used to take it per call.

use alloc::vec::Vec;

use crate::{
    Corrupt, TAG_LZ, TAG_LZ_DICT, TAG_LZH, TAG_LZH_DICT, TAG_RAW, decode, huff, parse_dict,
    read_varint,
};

/// A dictionary, parsed once.
///
/// The dictionary is per-FILE state — `kevy-vlog` trains one at rotation
/// and every record in that file decodes against it — and every entry
/// point here used to take it as a per-call argument. That cost three
/// things per record: unpacking 128 header bytes and Kraft-validating
/// 256 lengths, rebuilding an 8 KiB Huffman decode table, and (on the
/// encode side) re-hashing all 65,532 dictionary positions.
///
/// Holding one of these moves that work to where the state actually
/// changes. It is the same bytes and the same frames — no byte of the
/// format moves — so a `Dict` and a `&[u8]` decode identically.
///
/// ```
/// use kevy_compress::{Dict, encode, decode, decode_with, train};
///
/// let vals: Vec<&[u8]> = vec![b"user=alice role=admin", b"user=bob role=admin"];
/// let raw = train(&vals, kevy_compress::MAX_OFFSET);
/// let frame = encode(&raw, b"user=carol role=admin");
///
/// let d = Dict::new(&raw);
/// assert_eq!(decode_with(&d, &frame).unwrap(), decode(&raw, &frame).unwrap());
/// ```
pub struct Dict {
    lens: Option<[u8; 256]>,
    content: Vec<u8>,
    table: Option<huff::DecodeTable>,
    seeded: Vec<u32>,
}

impl core::fmt::Debug for Dict {
    /// The shape, not the bytes: a dictionary is up to 64 KiB and its
    /// decode table another 8, and a `{:?}` that pastes those into a log
    /// line is worse than no `Debug` at all.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Dict")
            .field("content_bytes", &self.content.len())
            .field("has_entropy_table", &self.table.is_some())
            .field("match_table_slots", &self.seeded.len())
            .finish()
    }
}

impl Dict {
    /// Parse `bytes` and build what every record would otherwise rebuild.
    ///
    /// Eager on purpose: this is constructed once per file, so paying the
    /// table build here is paying it once instead of once per record.
    /// [`decode`] does NOT construct one — it would then build an 8 KiB
    /// table to decode a `TAG_RAW` frame that never looks at it.
    /// Owning rather than borrowing so a `Dict` can be stored beside the
    /// file it belongs to — `kevy-vlog`'s `VlogFile` would otherwise be
    /// self-referential. The copy is one dictionary per file.
    #[must_use]
    pub fn new(bytes: &[u8]) -> Self {
        let (lens, content) = parse_dict(bytes);
        let table = lens.as_ref().map(huff::DecodeTable::new);
        let seeded = crate::encode::seeded_table(content);
        Self { lens, content: content.to_vec(), table, seeded }
    }

    /// The dictionary content, with any header stripped.
    #[must_use]
    pub fn content(&self) -> &[u8] {
        &self.content
    }
}

/// [`decode`] against a dictionary that was parsed once.
///
/// Identical output to `decode(bytes, frame)` for the same bytes — this
/// changes when the per-file work happens, not what any frame means.
///
/// ```
/// use kevy_compress::{Dict, decode, decode_with, encode_high, train};
///
/// let vals: Vec<&[u8]> = vec![b"level=info svc=api", b"level=warn svc=api"];
/// let raw = train(&vals, kevy_compress::MAX_OFFSET);
/// let frame = encode_high(&raw, b"level=error svc=api");
///
/// // The compaction path is the one that rebuilt an 8 KiB table per
/// // record; same bytes out either way.
/// let d = Dict::new(&raw);
/// assert_eq!(decode_with(&d, &frame).unwrap(), b"level=error svc=api");
/// assert_eq!(decode_with(&d, &frame).unwrap(), decode(&raw, &frame).unwrap());
///
/// // A `Dict` says its shape and not its contents.
/// assert!(format!("{d:?}").starts_with("Dict {"));
/// ```
///
/// # Errors
/// [`Corrupt`] when the frame does not decode to exactly what its header
/// promises, the same conditions as [`decode`].
pub fn decode_with(dict: &Dict, frame: &[u8]) -> Result<Vec<u8>, Corrupt> {
    let (&tag, rest) = frame.split_first().ok_or(Corrupt)?;
    let (orig_len, payload) = read_varint(rest)?;
    match tag {
        TAG_RAW if payload.len() == orig_len => Ok(payload.to_vec()),
        TAG_RAW => Err(Corrupt),
        TAG_LZ => decode::lz(&[], payload, orig_len),
        TAG_LZ_DICT if dict.content.is_empty() => Err(Corrupt),
        TAG_LZ_DICT => decode::lz(&dict.content, payload, orig_len),
        // The 5.0.0 compat retry, as in `decode`: that encoder could emit
        // a shared-table literal block under the dict-less tag.
        TAG_LZH => match decode::lz_high(&[], None, None, payload, orig_len) {
            Err(Corrupt) if dict.lens.is_some() => {
                decode::lz_high(&[], dict.lens.as_ref(), dict.table.as_ref(), payload, orig_len)
            }
            r => r,
        },
        TAG_LZH_DICT if dict.content.is_empty() => Err(Corrupt),
        TAG_LZH_DICT => decode::lz_high(
            &dict.content,
            dict.lens.as_ref(),
            dict.table.as_ref(),
            payload,
            orig_len,
        ),
        _ => Err(Corrupt),
    }
}

/// [`crate::encode`] against a dictionary whose match table was seeded
/// once.
///
/// Seeding walks every dictionary position — 65,532 hash-and-store for
/// the 64 KiB dictionary `kevy-vlog` trains — and it happened on every
/// record, which is why encode time was flat in input size: an 8-byte
/// value cost more than a 6 KiB one. Here it is a memcpy of a 16 KiB
/// table instead.
///
/// Same frames as [`crate::encode`] for the same bytes — asserted here,
/// because a speedup that changed what was written would be a different
/// change.
///
/// ```
/// use kevy_compress::{Dict, encode, encode_with, train};
///
/// let vals: Vec<&[u8]> = vec![b"user=alice role=admin", b"user=bob role=admin"];
/// let raw = train(&vals, kevy_compress::MAX_OFFSET);
/// let d = Dict::new(&raw);
/// let v = b"user=carol role=admin";
/// assert_eq!(encode_with(&d, v), encode(&raw, v));
/// ```
#[must_use]
pub fn encode_with(dict: &Dict, input: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(input.len() + crate::MAX_HEADER);
    let (tag, ok) = if input.len() >= crate::encode::MIN_INPUT {
        crate::encode::try_lz(&dict.content, Some(&dict.seeded), input, &mut frame)
    } else {
        (TAG_RAW, false)
    };
    crate::finish_or_raw(&mut frame, tag, ok, input);
    frame
}

/// [`crate::encode_high`] against a dictionary parsed and seeded once.
///
/// Same frames as [`crate::encode_high`] for the same bytes.
///
/// ```
/// use kevy_compress::{Dict, decode, encode_high, encode_high_with, train};
///
/// let vals: Vec<&[u8]> = vec![b"GET /a 200", b"GET /b 200", b"GET /c 404"];
/// let raw = train(&vals, kevy_compress::MAX_OFFSET);
/// let d = Dict::new(&raw);
/// let v = b"GET /d 200";
/// assert_eq!(encode_high_with(&d, v), encode_high(&raw, v));
/// assert_eq!(decode(&raw, &encode_high_with(&d, v)).unwrap(), v);
/// ```
#[must_use]
pub fn encode_high_with(dict: &Dict, input: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(input.len() + crate::MAX_HEADER);
    let (tag, ok) = if input.len() >= crate::encode::MIN_INPUT {
        crate::encode::try_high(
            &dict.content,
            dict.lens.as_ref(),
            Some(&dict.seeded),
            input,
            &mut frame,
        )
    } else {
        (TAG_RAW, false)
    };
    crate::finish_or_raw(&mut frame, tag, ok, input);
    frame
}

#[cfg(test)]
mod tests {
    use super::{Dict, decode_with, encode_high_with, encode_with};

    fn dict_bytes() -> Vec<u8> {
        let vals: Vec<&[u8]> = vec![b"level=info svc=api dur=12", b"level=warn svc=api dur=48"];
        crate::train(&vals, crate::MAX_OFFSET)
    }

    /// The prebuilt table has to be the one that gets used, and only a
    /// dictionary-carried literal block (flag 2) reaches it — which is
    /// why the arm that takes it went from ten never-executed regions
    /// to eleven when it was added without a test.
    #[test]
    fn a_prebuilt_table_decodes_the_same_bytes_as_a_rebuilt_one() {
        let raw = dict_bytes();
        let d = Dict::new(&raw);
        for v in [
            b"level=error svc=api dur=99".as_slice(),
            b"level=info svc=api dur=7",
            b"unrelated bytes entirely",
        ] {
            let high = encode_high_with(&d, v);
            assert_eq!(high, crate::encode_high(&raw, v), "frames must not move");
            assert_eq!(decode_with(&d, &high).unwrap(), v);
            assert_eq!(crate::decode(&raw, &high).unwrap(), v);

            let fast = encode_with(&d, v);
            assert_eq!(fast, crate::encode(&raw, v), "frames must not move");
            assert_eq!(decode_with(&d, &fast).unwrap(), v);
        }
    }

    /// A `Dict` over bytes that are not a dictionary still decodes the
    /// frames those bytes produced — the no-magic path.
    #[test]
    fn plain_bytes_are_a_dictionary_without_an_entropy_table() {
        let raw = b"a plain shared prefix, no magic".to_vec();
        let d = Dict::new(&raw);
        assert!(!format!("{d:?}").contains("true"), "no entropy table here: {d:?}");
        let v = b"a plain shared prefix, and then some";
        assert_eq!(decode_with(&d, &encode_with(&d, v)).unwrap(), v);
    }

    /// Corrupt input is refused, not guessed at, through this entry
    /// point too.
    #[test]
    fn a_frame_that_is_not_one_is_refused() {
        let d = Dict::new(&dict_bytes());
        assert!(decode_with(&d, b"").is_err());
        assert!(decode_with(&d, b"\xff\xff\xff").is_err());
    }
}

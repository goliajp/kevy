//! The AOF v2 record envelope: `[u32-LE payload_len][u32-LE crc32c][payload]`.
//!
//! One format move buys four things the bare-RESP v1 stream could not give:
//! deterministic resync after a corrupt frame (scan forward, validate
//! len+CRC — false accept ~2⁻³² per candidate), streaming replay (record
//! boundaries without parsing RESP), torn-tail detection without a full
//! parse, and integrity — a flipped payload bit fails the CRC instead of
//! replaying silently (crashgate's payload-flip cell caught v1 doing
//! exactly that). v1 files stay readable forever; a rewrite upgrades them.

use std::io::{self, Write};

use kevy_resp::ArgvView;

use crate::crc32c::crc32c;
use crate::rewrite_fmt::write_multibulk;

/// v2 file magic. Same length as the v1 `KEVYAOF1\n`, so the format sniff
/// reads one fixed-size header.
///
/// ```
/// use kevy_persist::{AOF_MAGIC, AOF2_MAGIC, Aof, AofFormat, Fsync};
///
/// assert_eq!(AOF2_MAGIC.len(), AOF_MAGIC.len());
/// let path = std::env::temp_dir().join(format!("aof2-magic-doc-{}.aof", std::process::id()));
/// let aof = Aof::open(&path, Fsync::No)?;
/// assert_eq!(aof.format(), AofFormat::V2);
/// assert_eq!(std::fs::read(&path)?, AOF2_MAGIC); // every new log starts so
/// # drop(aof);
/// # std::fs::remove_file(&path)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub const AOF2_MAGIC: &[u8; 9] = b"KEVYAOF2\n";

/// Which on-disk AOF encoding a file (or an image producer) speaks.
/// v1 = bare RESP stream (3.x and the wasm host-mediated log); v2 = the
/// checksummed record envelope. Readers accept both forever; the engine
/// writes v2 for every new file and upgrades v1 files on rewrite.
///
/// ```
/// use kevy_persist::{AOF_MAGIC, AOF2_MAGIC, AofFormat, dump_store_to_buf};
///
/// let store = kevy_store::Store::new();
/// assert!(dump_store_to_buf(&store, AofFormat::V1).0.starts_with(AOF_MAGIC));
/// assert!(dump_store_to_buf(&store, AofFormat::V2).0.starts_with(AOF2_MAGIC));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AofFormat {
    /// Bare RESP command stream (`KEVYAOF1\n` or legacy headerless).
    ///
    /// ```
    /// use kevy_persist::{Aof, AofFormat, Fsync};
    ///
    /// let path = std::env::temp_dir().join(format!("format-v1-doc-{}.aof", std::process::id()));
    /// std::fs::write(&path, kevy_persist::AOF_MAGIC)?; // a 3.x log
    /// let mut aof = Aof::open(&path, Fsync::No)?;
    /// assert_eq!(aof.format(), AofFormat::V1, "appends stay v1 until a rewrite");
    /// aof.rewrite_from(&kevy_store::Store::new())?;
    /// assert_eq!(aof.format(), AofFormat::V2);
    /// # drop(aof);
    /// # std::fs::remove_file(&path)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    V1,
    /// Checksummed record envelope (`KEVYAOF2\n`).
    ///
    /// ```
    /// use kevy_persist::{AOF2_MAGIC, AofFormat, RecordStep, dump_store_to_buf, next_record};
    /// use kevy_store::{SetCondition, Store};
    ///
    /// let mut store = Store::new();
    /// store.set(b"k", b"v".to_vec(), None, SetCondition::Always);
    /// let (image, keys) = dump_store_to_buf(&store, AofFormat::V2);
    /// assert_eq!(keys, 1);
    /// // after the magic, records walk without parsing RESP
    /// assert!(matches!(next_record(&image, AOF2_MAGIC.len()), RecordStep::Ok { .. }));
    /// ```
    V2,
}

/// Envelope header size: u32 payload length + u32 CRC32C.
pub(crate) const RECORD_HEADER: usize = 8;

/// Sanity cap on a single record's payload (a corrupt length field must not
/// drive a multi-GB allocation): generous for any real command — a 512 MiB
/// value plus framing.
pub(crate) const MAX_RECORD: u32 = 1 << 30;

/// Append one enveloped record wrapping `payload`.
pub(crate) fn write_record<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(&crc32c(payload).to_le_bytes())?;
    w.write_all(payload)
}

/// Encode `args` as a RESP multibulk and append it as one enveloped record,
/// reusing `scratch` for the payload bytes (cleared, not shrunk).
///
/// Public for external v2-stream producers (the wasm door's
/// host-mediated pump encodes its outbound frames with this) — the
/// on-disk writer inside this crate uses it too, so there is exactly
/// one encoding of a record.
///
/// ```
/// use kevy_persist::{Argv, RecordStep, next_record, write_record_multibulk};
///
/// let set = Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
/// let mut image = Vec::new();
/// write_record_multibulk(&mut image, &set, &mut Vec::new())?;
/// let RecordStep::Ok { payload, consumed } = next_record(&image, 0) else { panic!() };
/// assert_eq!(payload, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n");
/// assert_eq!(consumed, image.len());
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn write_record_multibulk<W: Write, A: ArgvView + ?Sized>(
    mut w: W,
    args: &A,
    scratch: &mut Vec<u8>,
) -> io::Result<()> {
    scratch.clear();
    write_multibulk(&mut *scratch, args)?;
    write_record(&mut w, scratch)
}

/// One step of a v2 record walk over an in-memory image.
///
/// Public for external incremental consumers ([`next_record`]): a
/// stream arriving in arbitrary chunks treats `Truncated` as "wait
/// for more bytes" and `Corrupt` as its format error.
///
/// ```
/// use kevy_persist::{Argv, RecordStep, next_record, write_record_multibulk};
///
/// let set = Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
/// let mut image = Vec::new();
/// write_record_multibulk(&mut image, &set, &mut Vec::new())?;
/// // a consumer fed in chunks waits on `Truncated`, stops on `Corrupt`
/// let mut commands = 0;
/// let mut pos = 0;
/// while pos < image.len() {
///     match next_record(&image, pos) {
///         RecordStep::Ok { consumed, .. } => (commands, pos) = (commands + 1, pos + consumed),
///         RecordStep::Truncated | RecordStep::Corrupt => break,
///     }
/// }
/// assert_eq!(commands, 1);
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug)]
pub enum RecordStep<'a> {
    /// A complete, checksum-valid record.
    ///
    /// ```
    /// use kevy_persist::{Argv, RecordStep, next_record, write_record_multibulk};
    ///
    /// let set = Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// let mut image = Vec::new();
    /// write_record_multibulk(&mut image, &set, &mut Vec::new())?;
    /// assert!(matches!(next_record(&image, 0), RecordStep::Ok { .. }));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    Ok {
        /// The record's payload (one RESP multibulk command).
        ///
        /// ```
        /// use kevy_persist::{Argv, RecordStep, next_record, write_record_multibulk};
        ///
        /// let del = Argv::from(vec![b"DEL".to_vec(), b"k".to_vec()]);
        /// let mut image = Vec::new();
        /// write_record_multibulk(&mut image, &del, &mut Vec::new())?;
        /// let RecordStep::Ok { payload, .. } = next_record(&image, 0) else { panic!() };
        /// let (parsed, _) = kevy_resp::parse_command(payload)?.ok_or("incomplete")?;
        /// assert_eq!(parsed, del);
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        payload: &'a [u8],
        /// Total bytes consumed from the buffer (header + payload).
        ///
        /// ```
        /// use kevy_persist::{Argv, RecordStep, next_record, write_record_multibulk};
        ///
        /// let del = Argv::from(vec![b"DEL".to_vec(), b"k".to_vec()]);
        /// let mut image = Vec::new();
        /// write_record_multibulk(&mut image, &del, &mut Vec::new())?;
        /// let RecordStep::Ok { payload, consumed } = next_record(&image, 0) else { panic!() };
        /// assert_eq!(consumed, 8 + payload.len()); // length + checksum header
        /// # Ok::<(), std::io::Error>(())
        /// ```
        consumed: usize,
    },
    /// The buffer ends mid-record (torn tail): not an error, the prefix
    /// before this record is the replayable part.
    ///
    /// ```
    /// use kevy_persist::{Argv, RecordStep, next_record, write_record_multibulk};
    ///
    /// let set = Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// let mut image = Vec::new();
    /// write_record_multibulk(&mut image, &set, &mut Vec::new())?;
    /// image.pop(); // a crash mid-append
    /// assert!(matches!(next_record(&image, 0), RecordStep::Truncated));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    Truncated,
    /// The record is structurally present but lies: oversized length or a
    /// checksum mismatch. Everything from here on is non-replayable.
    ///
    /// ```
    /// use kevy_persist::{Argv, RecordStep, next_record, write_record_multibulk};
    ///
    /// let set = Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// let mut image = Vec::new();
    /// write_record_multibulk(&mut image, &set, &mut Vec::new())?;
    /// *image.last_mut().unwrap() ^= 0x20; // one flipped bit
    /// assert!(matches!(next_record(&image, 0), RecordStep::Corrupt));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    Corrupt,
}

/// Inspect the record starting at `buf[pos..]`.
///
/// # Panics
///
/// Panics if `pos > buf.len()`.
///
/// ```
/// use kevy_persist::{Argv, RecordStep, next_record, write_record_multibulk};
///
/// let mut image = Vec::new();
/// let mut scratch = Vec::new();
/// for key in [b"a", b"b"] {
///     let set = Argv::from(vec![b"SET".to_vec(), key.to_vec(), b"v".to_vec()]);
///     write_record_multibulk(&mut image, &set, &mut scratch)?;
/// }
/// let RecordStep::Ok { consumed, .. } = next_record(&image, 0) else { panic!() };
/// assert!(matches!(next_record(&image, consumed), RecordStep::Ok { .. }));
/// assert!(matches!(next_record(&image, image.len()), RecordStep::Truncated)); // clean end
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn next_record(buf: &[u8], pos: usize) -> RecordStep<'_> {
    let rest = &buf[pos..];
    if rest.is_empty() {
        return RecordStep::Truncated; // clean end is the caller's pos == len check
    }
    if rest.len() < RECORD_HEADER {
        return RecordStep::Truncated;
    }
    let len = u32::from_le_bytes(
        rest[..4].try_into().expect("the rest.len() < RECORD_HEADER return above"),
    );
    if len == 0 || len > MAX_RECORD {
        return RecordStep::Corrupt;
    }
    let crc = u32::from_le_bytes(
        rest[4..8].try_into().expect("the rest.len() < RECORD_HEADER return above"),
    );
    let Some(payload) = rest.get(RECORD_HEADER..RECORD_HEADER + len as usize) else {
        return RecordStep::Truncated;
    };
    if crc32c(payload) != crc {
        return RecordStep::Corrupt;
    }
    RecordStep::Ok { payload, consumed: RECORD_HEADER + len as usize }
}

/// Scan forward from `from` for the next offset that starts a valid,
/// checksummed, exactly-one-command record — the deterministic resync a
/// corrupt record cannot defeat: a false accept needs a byte position
/// whose length field fits, whose CRC32C matches (~2⁻³²), AND whose
/// payload parses as one complete command. Returns `None` when only a
/// torn tail (or nothing) remains.
pub(crate) fn resync_scan(buf: &[u8], from: usize) -> Option<usize> {
    let mut q = from;
    while q < buf.len() {
        match next_record(buf, q) {
            RecordStep::Ok { payload, .. } => {
                if matches!(
                    kevy_resp::parse_command(payload),
                    Ok(Some((_, used))) if used == payload.len()
                ) {
                    return Some(q);
                }
                q += 1;
            }
            // Truncated here does NOT mean the tail is torn: a garbage
            // window can fake a plausible length that runs past the end
            // (seen in the first test run — an ASCII byte as the length's
            // high byte reads as ~600 MB). Keep scanning; the loop bound
            // is the real end.
            RecordStep::Truncated | RecordStep::Corrupt => q += 1,
        }
    }
    None
}

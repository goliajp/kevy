//! Stream entry ids and the parsers for their wire forms.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;

/// A stream entry's `<ms>-<seq>` identifier. The `Ord` derivation compares
/// `ms` first then `seq`, which is exactly the monotonic order the protocol
/// requires; same derivation gives `Eq`, `Hash`, and the `BTreeMap` key bound.
///
/// ```
/// use kevy_store::StreamId;
/// let id = StreamId::new(5, 2);
/// assert_eq!((id.ms, id.seq), (5, 2));
/// assert_eq!(id.encode(), b"5-2");
/// assert!(id < StreamId::new(6, 0));
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Default)]
#[non_exhaustive]
pub struct StreamId {
    /// Unix milliseconds timestamp component.
    ///
    /// ```
    /// let id = kevy_store::parse_explicit_id(b"1700000000000-3")?;
    /// assert_eq!(id.ms, 1_700_000_000_000);
    /// # Ok::<(), kevy_store::StreamIdError>(())
    /// ```
    pub ms: u64,
    /// Per-ms sequence number, 0-based.
    ///
    /// ```
    /// use kevy_store::StreamId;
    /// assert_eq!(kevy_store::parse_explicit_id(b"5-3")?.seq, 3);
    /// assert_eq!(StreamId::new(5, 3).next().seq, 4);
    /// # Ok::<(), kevy_store::StreamIdError>(())
    /// ```
    pub seq: u64,
}

impl StreamId {
    /// The numerically smallest ID; XRANGE `-` start.
    pub const MIN: StreamId = StreamId::new(0, 0);
    /// The numerically largest representable ID; XRANGE `+` end.
    pub const MAX: StreamId = StreamId::new(u64::MAX, u64::MAX);

    /// The ID `<ms>-<seq>`.
    ///
    /// ```
    /// assert_eq!(kevy_store::StreamId::new(0, 0), kevy_store::StreamId::MIN);
    /// ```
    pub const fn new(ms: u64, seq: u64) -> Self {
        Self { ms, seq }
    }

    /// Render as the canonical `<ms>-<seq>` wire form.
    pub fn encode(self) -> Vec<u8> {
        format!("{}-{}", self.ms, self.seq).into_bytes()
    }

    /// Step one ID past `self`. Saturates at [`Self::MAX`].
    #[must_use]
    pub fn next(self) -> Self {
        if self.seq < u64::MAX {
            StreamId::new(self.ms, self.seq + 1)
        } else if self.ms < u64::MAX {
            StreamId::new(self.ms + 1, 0)
        } else {
            StreamId::MAX
        }
    }
}

/// XADD's ID argument: either an explicit `<ms>-<seq>` (both parts may
/// be `*` to auto-fill `seq` only) or fully auto-generate via `*`.
///
/// ```
/// use kevy_store::{StreamId, XAddIdSpec, parse_xadd_id};
/// assert_eq!(parse_xadd_id(b"*"), Ok(XAddIdSpec::AutoAll));
/// assert_eq!(parse_xadd_id(b"7-*"), Ok(XAddIdSpec::AutoSeq(7)));
/// assert_eq!(parse_xadd_id(b"7-1"), Ok(XAddIdSpec::Explicit(StreamId::new(7, 1))));
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum XAddIdSpec {
    /// `*` — generate both `ms` (= current wall-clock) and `seq`.
    ///
    /// ```
    /// use kevy_store::{StreamData, StreamId, XAddIdSpec};
    /// let stream = StreamData::default();
    /// // `now_ms` becomes the ms part; seq starts at 0 in a fresh millisecond
    /// assert_eq!(stream.resolve_xadd_id(XAddIdSpec::AutoAll, 42)?, StreamId::new(42, 0));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    AutoAll,
    /// `<ms>-*` — caller fixes `ms`, server picks the next free `seq`.
    ///
    /// ```
    /// use kevy_store::{StreamData, StreamId, XAddIdSpec};
    /// let stream = StreamData::default();
    /// assert_eq!(stream.resolve_xadd_id(XAddIdSpec::AutoSeq(7), 42)?, StreamId::new(7, 0));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    AutoSeq(u64),
    /// `<ms>-<seq>` — caller fully specifies the ID.
    ///
    /// ```
    /// use kevy_store::{StreamData, StreamId, XAddIdSpec};
    /// let stream = StreamData::default();
    /// let id = StreamId::new(5, 5);
    /// assert_eq!(stream.resolve_xadd_id(XAddIdSpec::Explicit(id), 42)?, id);
    /// // 0-0 is never a valid entry id
    /// assert!(stream.resolve_xadd_id(XAddIdSpec::Explicit(StreamId::MIN), 42).is_err());
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Explicit(StreamId),
}

/// Parse an XADD ID argument (`*`, `ms`, `ms-*`, `ms-seq`).
///
/// ```
/// use kevy_store::{StreamId, XAddIdSpec, parse_xadd_id};
/// assert_eq!(parse_xadd_id(b"*")?, XAddIdSpec::AutoAll);
/// assert_eq!(parse_xadd_id(b"9")?, XAddIdSpec::Explicit(StreamId::new(9, 0)));
/// assert!(parse_xadd_id(b"9-x").is_err());
/// # Ok::<(), kevy_store::StreamIdError>(())
/// ```
pub fn parse_xadd_id(s: &[u8]) -> Result<XAddIdSpec, StreamIdError> {
    if s == b"*" {
        return Ok(XAddIdSpec::AutoAll);
    }
    let txt = core::str::from_utf8(s).map_err(|_| StreamIdError::Invalid)?;
    match txt.split_once('-') {
        None => {
            let ms = txt.parse::<u64>().map_err(|_| StreamIdError::Invalid)?;
            Ok(XAddIdSpec::Explicit(StreamId::new(ms, 0)))
        }
        Some((ms_s, seq_s)) => {
            let ms = ms_s.parse::<u64>().map_err(|_| StreamIdError::Invalid)?;
            if seq_s == "*" {
                Ok(XAddIdSpec::AutoSeq(ms))
            } else {
                let seq = seq_s.parse::<u64>().map_err(|_| StreamIdError::Invalid)?;
                Ok(XAddIdSpec::Explicit(StreamId::new(ms, seq)))
            }
        }
    }
}

/// Parse an XRANGE `start` ID. Accepts `-` (= [`StreamId::MIN`]), bare
/// `ms` (seq=0), and full `ms-seq`.
///
/// ```
/// use kevy_store::{StreamId, parse_range_start};
/// assert_eq!(parse_range_start(b"-")?, StreamId::MIN);
/// assert_eq!(parse_range_start(b"5")?, StreamId::new(5, 0));
/// # Ok::<(), kevy_store::StreamIdError>(())
/// ```
pub fn parse_range_start(s: &[u8]) -> Result<StreamId, StreamIdError> {
    if s == b"-" {
        return Ok(StreamId::MIN);
    }
    parse_explicit_id(s)
}

/// Parse an XRANGE `end` ID. Accepts `+` (= [`StreamId::MAX`]), bare `ms`
/// (seq=u64::MAX so the entire ms is included), and full `ms-seq`.
///
/// ```
/// use kevy_store::{StreamId, parse_range_end};
/// assert_eq!(parse_range_end(b"+")?, StreamId::MAX);
/// // a bare ms covers the whole millisecond
/// assert_eq!(parse_range_end(b"5")?, StreamId::new(5, u64::MAX));
/// # Ok::<(), kevy_store::StreamIdError>(())
/// ```
pub fn parse_range_end(s: &[u8]) -> Result<StreamId, StreamIdError> {
    if s == b"+" {
        return Ok(StreamId::MAX);
    }
    parse_id(s, u64::MAX)
}

/// Parse a fully-explicit ID for XREAD's per-stream "last-seen" arg
/// (`0`, `0-0`, `5-2`). `$` is handled by the caller (it means "the
/// stream's current `last_id`", which only Store can resolve).
///
/// A bare `ms` means `<ms>-0`, the first ID of that millisecond.
///
/// ```
/// use kevy_store::{StreamId, parse_explicit_id};
/// assert_eq!(parse_explicit_id(b"5"), Ok(StreamId::new(5, 0)));
/// assert_eq!(parse_explicit_id(b"5-2"), Ok(StreamId::new(5, 2)));
/// assert!(parse_explicit_id(b"$").is_err());
/// ```
pub fn parse_explicit_id(s: &[u8]) -> Result<StreamId, StreamIdError> {
    parse_id(s, 0)
}

/// `<ms>[-<seq>]`, with `bare_seq` standing in for a missing `-<seq>`.
fn parse_id(s: &[u8], bare_seq: u64) -> Result<StreamId, StreamIdError> {
    let txt = core::str::from_utf8(s).map_err(|_| StreamIdError::Invalid)?;
    let (ms_s, seq) = match txt.split_once('-') {
        Some((ms_s, seq_s)) => (ms_s, seq_s.parse::<u64>().map_err(|_| StreamIdError::Invalid)?),
        None => (txt, bare_seq),
    };
    let ms = ms_s.parse::<u64>().map_err(|_| StreamIdError::Invalid)?;
    Ok(StreamId::new(ms, seq))
}

/// Errors `parse_*_id` may emit. Distinct from `StoreError::NotInteger`
/// so callers can map to the more specific Redis wire shape (`ERR
/// Invalid stream ID specified as stream command argument`).
///
/// ```
/// let e = kevy_store::parse_explicit_id(b"x").unwrap_err();
/// assert_eq!(e.to_string(), "invalid stream id");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StreamIdError {
    /// Couldn't parse the bytes as `<ms>[-<seq>]` / `*` / `-` / `+`.
    ///
    /// ```
    /// use kevy_store::{StreamIdError, parse_range_start};
    /// assert_eq!(parse_range_start(b"not-an-id"), Err(StreamIdError::Invalid));
    /// ```
    Invalid,
}

impl core::fmt::Display for StreamIdError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Invalid => f.write_str("invalid stream id"),
        }
    }
}

impl core::error::Error for StreamIdError {}

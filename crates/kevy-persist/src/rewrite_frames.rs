//! `value_as_v1_frames` — the rewrite serializer, reachable by callers
//! that need the commands rather than an AOF file. Split from
//! `rewrite_fmt` for the 500-LOC house rule.

// `write!` into an in-memory buffer returns a `Result` because
// `fmt::Write` / `io::Write` must, not because it can fail: the
// `String` and `Vec` impls are infallible. Said once here.
#![expect(clippy::let_underscore_must_use, reason = "writing to an in-memory buffer cannot fail")]

use kevy_store::Value;

use crate::rewrite_fmt::write_value_as_commands;

/// Emit one (or two, if TTL'd) RESP write commands that, when replayed,
/// reconstruct `key`'s `value` and TTL exactly, into a fresh buffer in
/// **V1 (plain RESP)** framing — parseable by
/// `kevy_resp::parse_command_into`.
///
/// The rewrite path calls the writer below directly. This wrapper is for
/// callers that need the *commands* rather than an AOF file: the
/// cross-shard RENAME has to record the value it just placed on another
/// shard, and reproducing the per-type mapping there would be a second
/// implementation of the one thing `BGREWRITEAOF` already has to get
/// right for every `Value` variant, TTL and stream shape.
///
/// ```
/// use kevy_persist::SnapshotSource;
/// use kevy_store::{SetCondition, Store};
/// use std::time::Duration;
///
/// let mut store = Store::new();
/// store.set(b"k", b"v".to_vec(), Some(Duration::from_secs(60)), SetCondition::Always);
/// let mut frames = Vec::new();
/// store.for_each_entry(|key, value, ttl| frames = kevy_persist::value_as_v1_frames(key, value, ttl));
/// // SET, then the deadline as an absolute PEXPIREAT
/// let (set, used) = kevy_resp::parse_command(&frames)?.ok_or("incomplete")?;
/// assert_eq!(set, vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
/// let (ttl, _) = kevy_resp::parse_command(&frames[used..])?.ok_or("incomplete")?;
/// assert_eq!(&ttl[0], b"PEXPIREAT");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn value_as_v1_frames(key: &[u8], value: &Value, ttl_ms: Option<u64>) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut scratch = Vec::new();
    let _ = write_value_into_vec(&mut buf, key, value, ttl_ms, crate::AofFormat::V1, &mut scratch);
    buf
}

/// The serializer's in-memory instance, compiled once here. Generic
/// callers (`dump_store_to_buf` is instantiated in the crate that calls
/// it) reach it through this function rather than instantiating the
/// writer themselves, which gave the linked program a second copy of it.
pub(crate) fn write_value_into_vec(
    buf: &mut Vec<u8>,
    key: &[u8],
    value: &Value,
    ttl_ms: Option<u64>,
    fmt: crate::AofFormat,
    scratch: &mut Vec<u8>,
) -> std::io::Result<()> {
    write_value_as_commands(buf, key, value, ttl_ms, fmt, scratch)
}

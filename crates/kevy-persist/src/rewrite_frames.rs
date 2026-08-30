//! `value_as_v1_frames` — the rewrite serializer, reachable by callers
//! that need the commands rather than an AOF file. Split from
//! `rewrite_fmt` for the 500-LOC house rule.

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
pub fn value_as_v1_frames(key: &[u8], value: &Value, ttl_ms: Option<u64>) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut scratch = Vec::new();
    let _ =
        write_value_as_commands(&mut buf, key, value, ttl_ms, crate::AofFormat::V1, &mut scratch);
    buf
}

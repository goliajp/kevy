//! Replication handshake — the first round-trip a replica makes
//! against the primary's replication TCP listener.
//!
//! Wire shape:
//!
//! - Replica → primary: a RESP2 multi-bulk command
//!   `REPLICATE FROM <from-offset> ID <replica-id>` (5 args). The
//!   `<from-offset>` is `0` for a fresh replica (full-sync intent) or
//!   the last applied offset from a reconnecting replica.
//! - Primary → replica: a RESP2 simple string `+ACK <current-offset>`,
//!   where `<current-offset>` is the primary's `next_offset` at the
//!   moment of ack. The replica records it and starts consuming live
//!   frames; if the primary's [`crate::source::ReplicationSource`]
//!   cannot serve from `<from-offset>` (TooOld), the primary instead
//!   begins a snapshot ship (handled by the wiring layer, not here).
//!
//! This module owns only the parse + format primitives. Socket I/O,
//! retry, and "did the primary choose snapshot vs live stream" logic
//! live in the future replication source/replica modules.

use kevy_resp::Argv;

/// Parsed `REPLICATE FROM <from-offset> ID <replica-id>` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeReq {
    /// Offset the replica wants to resume from. `0` = fresh replica.
    pub from_offset: u64,
    /// Replica-supplied identifier (operator-set, opaque to the
    /// primary other than for slot bookkeeping).
    pub replica_id: String,
}

/// Why a [`parse_replicate_from`] call rejected its input.
#[derive(Debug, PartialEq, Eq)]
pub enum HandshakeError {
    /// First arg is not "REPLICATE" (case-insensitive).
    BadCommand,
    /// Argument count is not exactly 5.
    WrongArity(usize),
    /// Second arg is not "FROM" (case-insensitive).
    BadFromKeyword,
    /// Third arg did not parse as an unsigned decimal `u64`.
    BadOffset,
    /// Fourth arg is not "ID" (case-insensitive).
    BadIdKeyword,
    /// Replica id is empty or not valid UTF-8.
    BadReplicaId,
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadCommand => write!(f, "expected REPLICATE command"),
            Self::WrongArity(n) => write!(f, "REPLICATE expects 5 args, got {n}"),
            Self::BadFromKeyword => write!(f, "expected 'FROM' keyword"),
            Self::BadOffset => write!(f, "from-offset must be an unsigned decimal"),
            Self::BadIdKeyword => write!(f, "expected 'ID' keyword"),
            Self::BadReplicaId => write!(f, "replica id must be non-empty UTF-8"),
        }
    }
}

impl std::error::Error for HandshakeError {}

/// Parse a `REPLICATE FROM <offset> ID <id>` command from an
/// already-decoded [`Argv`] (the caller has run the bytes through
/// `kevy_resp::parse_command_into` first).
pub fn parse_replicate_from(argv: &Argv) -> Result<HandshakeReq, HandshakeError> {
    if argv.len() != 5 {
        return Err(HandshakeError::WrongArity(argv.len()));
    }
    if !eq_ascii_ci(argv.get(0).unwrap(), b"REPLICATE") {
        return Err(HandshakeError::BadCommand);
    }
    if !eq_ascii_ci(argv.get(1).unwrap(), b"FROM") {
        return Err(HandshakeError::BadFromKeyword);
    }
    let from_offset =
        parse_decimal_u64(argv.get(2).unwrap()).ok_or(HandshakeError::BadOffset)?;
    if !eq_ascii_ci(argv.get(3).unwrap(), b"ID") {
        return Err(HandshakeError::BadIdKeyword);
    }
    let id_bytes = argv.get(4).unwrap();
    if id_bytes.is_empty() {
        return Err(HandshakeError::BadReplicaId);
    }
    let replica_id =
        std::str::from_utf8(id_bytes).map_err(|_| HandshakeError::BadReplicaId)?.to_string();
    Ok(HandshakeReq {
        from_offset,
        replica_id,
    })
}

/// Encode the primary's `+ACK <current-offset>\r\n` response.
pub fn encode_ack(current_offset: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 20 + 2);
    out.extend_from_slice(b"+ACK ");
    push_u64(&mut out, current_offset);
    out.extend_from_slice(b"\r\n");
    out
}

fn eq_ascii_ci(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.eq_ignore_ascii_case(y))
}

fn parse_decimal_u64(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }
    let mut n: u64 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(u64::from(b - b'0'))?;
    }
    Some(n)
}

fn push_u64(out: &mut Vec<u8>, n: u64) {
    if n == 0 {
        out.push(b'0');
        return;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    let mut v = n;
    while v != 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    out.extend_from_slice(&tmp[i..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&[u8]]) -> Argv {
        let mut a = Argv::default();
        for arg in args {
            a.push(arg);
        }
        a
    }

    #[test]
    fn parses_fresh_replica_from_zero() {
        let req = parse_replicate_from(&argv(&[
            b"REPLICATE",
            b"FROM",
            b"0",
            b"ID",
            b"replica-a",
        ]))
        .unwrap();
        assert_eq!(req.from_offset, 0);
        assert_eq!(req.replica_id, "replica-a");
    }

    #[test]
    fn parses_reconnect_with_large_offset() {
        let req = parse_replicate_from(&argv(&[
            b"REPLICATE",
            b"FROM",
            b"4294967296", // 2^32 — guarantees u64 path
            b"ID",
            b"node-7",
        ]))
        .unwrap();
        assert_eq!(req.from_offset, 4_294_967_296);
        assert_eq!(req.replica_id, "node-7");
    }

    #[test]
    fn keywords_are_case_insensitive() {
        let req = parse_replicate_from(&argv(&[
            b"replicate", b"from", b"1", b"id", b"x",
        ]))
        .unwrap();
        assert_eq!(req.from_offset, 1);
        assert_eq!(req.replica_id, "x");
    }

    #[test]
    fn wrong_arity_rejected_with_actual_count() {
        let err = parse_replicate_from(&argv(&[b"REPLICATE", b"FROM", b"0"])).unwrap_err();
        assert_eq!(err, HandshakeError::WrongArity(3));
    }

    #[test]
    fn wrong_command_rejected() {
        let err =
            parse_replicate_from(&argv(&[b"SUBSCRIBE", b"FROM", b"0", b"ID", b"a"])).unwrap_err();
        assert_eq!(err, HandshakeError::BadCommand);
    }

    #[test]
    fn wrong_from_keyword_rejected() {
        let err =
            parse_replicate_from(&argv(&[b"REPLICATE", b"AT", b"0", b"ID", b"a"])).unwrap_err();
        assert_eq!(err, HandshakeError::BadFromKeyword);
    }

    #[test]
    fn wrong_id_keyword_rejected() {
        let err = parse_replicate_from(&argv(&[
            b"REPLICATE",
            b"FROM",
            b"0",
            b"NAME",
            b"a",
        ]))
        .unwrap_err();
        assert_eq!(err, HandshakeError::BadIdKeyword);
    }

    #[test]
    fn non_decimal_offset_rejected() {
        let err =
            parse_replicate_from(&argv(&[b"REPLICATE", b"FROM", b"NaN", b"ID", b"a"]))
                .unwrap_err();
        assert_eq!(err, HandshakeError::BadOffset);
    }

    #[test]
    fn negative_offset_rejected_as_bad_offset() {
        let err =
            parse_replicate_from(&argv(&[b"REPLICATE", b"FROM", b"-1", b"ID", b"a"]))
                .unwrap_err();
        assert_eq!(err, HandshakeError::BadOffset);
    }

    #[test]
    fn empty_replica_id_rejected() {
        let err =
            parse_replicate_from(&argv(&[b"REPLICATE", b"FROM", b"0", b"ID", b""]))
                .unwrap_err();
        assert_eq!(err, HandshakeError::BadReplicaId);
    }

    #[test]
    fn non_utf8_replica_id_rejected() {
        let err = parse_replicate_from(&argv(&[
            b"REPLICATE",
            b"FROM",
            b"0",
            b"ID",
            &[0xFF, 0xFE, 0xFD], // invalid UTF-8
        ]))
        .unwrap_err();
        assert_eq!(err, HandshakeError::BadReplicaId);
    }

    #[test]
    fn ack_format_for_zero() {
        assert_eq!(encode_ack(0), b"+ACK 0\r\n");
    }

    #[test]
    fn ack_format_for_large_offset() {
        assert_eq!(encode_ack(987_654_321), b"+ACK 987654321\r\n");
    }
}

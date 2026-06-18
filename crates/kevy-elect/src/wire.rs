//! Wire encode + decode for [`crate::Message`]. Uses kevy-resp's
//! RESP2 multi-bulk array shape, identical to the keyspace plane —
//! tcpdump-friendly text frames, single decoder path workspace-wide.

use kevy_resp::{ArgvBorrowed, parse_command_borrowed};

use crate::message::{Message, Role};

/// Encode a [`Message`] as a RESP2 multi-bulk array.
///
/// Numeric fields ride as decimal bulk strings — matches kevy-
/// replicate's `REPLICATE FROM <offset> ID <id>` handshake
/// convention. Pre-sized: every message ≤ 6 fields ≤ 32 bytes
/// each, so a 256-byte buffer suffices for every variant.
pub fn encode(msg: &Message) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    match msg {
        Message::Hb {
            epoch,
            node_id,
            role,
            repl_offset,
        } => {
            push_bulk_array(&mut out, 5);
            push_bulk(&mut out, b"HB");
            push_bulk(&mut out, epoch.to_string().as_bytes());
            push_bulk(&mut out, node_id.as_bytes());
            push_bulk(&mut out, role.as_str().as_bytes());
            push_bulk(&mut out, repl_offset.to_string().as_bytes());
        }
        Message::Offer {
            new_epoch,
            candidate_id,
            repl_offset,
        } => {
            push_bulk_array(&mut out, 4);
            push_bulk(&mut out, b"OFFER");
            push_bulk(&mut out, new_epoch.to_string().as_bytes());
            push_bulk(&mut out, candidate_id.as_bytes());
            push_bulk(&mut out, repl_offset.to_string().as_bytes());
        }
        Message::Accept { epoch, accepter_id } => {
            push_bulk_array(&mut out, 3);
            push_bulk(&mut out, b"ACCEPT");
            push_bulk(&mut out, epoch.to_string().as_bytes());
            push_bulk(&mut out, accepter_id.as_bytes());
        }
        Message::Announce {
            epoch,
            new_primary_id,
            new_primary_addr,
        } => {
            push_bulk_array(&mut out, 4);
            push_bulk(&mut out, b"ANNOUNCE");
            push_bulk(&mut out, epoch.to_string().as_bytes());
            push_bulk(&mut out, new_primary_id.as_bytes());
            push_bulk(&mut out, new_primary_addr.as_bytes());
        }
    }
    out
}

/// Errors `decode` can surface.
#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Buffer holds fewer bytes than the framed message header
    /// claims — read more from the socket and retry.
    Truncated,
    /// Bytes don't parse as a RESP multi-bulk (malformed envelope).
    Bad,
    /// Verb was missing, unknown, or had the wrong arity for its
    /// shape (e.g. `HB` with 3 args instead of 5).
    WrongShape,
    /// A numeric field (epoch / offset) was not a valid decimal
    /// `u64`.
    BadNumeric,
    /// A `role` field on `HB` was not one of `primary` / `replica`
    /// / `candidate`.
    BadRole,
}

/// Decode one [`Message`] off the front of `buf`. Returns the
/// decoded message and the number of bytes consumed. The caller
/// advances its read cursor by `consumed` on success, retries with
/// more bytes on `Truncated`, and drops the connection on every
/// other variant.
pub fn decode(buf: &[u8]) -> Result<(Message, usize), DecodeError> {
    let (argv, used) = match parse_command_borrowed(buf) {
        Ok(Some(pair)) => pair,
        Ok(None) => return Err(DecodeError::Truncated),
        Err(_) => return Err(DecodeError::Bad),
    };
    let verb = argv.first().ok_or(DecodeError::WrongShape)?;
    let msg = parse_argv_for_verb(verb, &argv)?;
    Ok((msg, used))
}

fn parse_argv_for_verb(verb: &[u8], argv: &ArgvBorrowed<'_>) -> Result<Message, DecodeError> {
    if verb.eq_ignore_ascii_case(b"HB") {
        if argv.len() != 5 {
            return Err(DecodeError::WrongShape);
        }
        Ok(Message::Hb {
            epoch: parse_u64(&argv[1])?,
            node_id: parse_string(&argv[2]),
            role: Role::parse(&argv[3]).ok_or(DecodeError::BadRole)?,
            repl_offset: parse_u64(&argv[4])?,
        })
    } else if verb.eq_ignore_ascii_case(b"OFFER") {
        if argv.len() != 4 {
            return Err(DecodeError::WrongShape);
        }
        Ok(Message::Offer {
            new_epoch: parse_u64(&argv[1])?,
            candidate_id: parse_string(&argv[2]),
            repl_offset: parse_u64(&argv[3])?,
        })
    } else if verb.eq_ignore_ascii_case(b"ACCEPT") {
        if argv.len() != 3 {
            return Err(DecodeError::WrongShape);
        }
        Ok(Message::Accept {
            epoch: parse_u64(&argv[1])?,
            accepter_id: parse_string(&argv[2]),
        })
    } else if verb.eq_ignore_ascii_case(b"ANNOUNCE") {
        if argv.len() != 4 {
            return Err(DecodeError::WrongShape);
        }
        Ok(Message::Announce {
            epoch: parse_u64(&argv[1])?,
            new_primary_id: parse_string(&argv[2]),
            new_primary_addr: parse_string(&argv[3]),
        })
    } else {
        Err(DecodeError::WrongShape)
    }
}

fn parse_u64(b: &[u8]) -> Result<u64, DecodeError> {
    std::str::from_utf8(b)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or(DecodeError::BadNumeric)
}

fn parse_string(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn push_bulk_array(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(format!("*{n}\r\n").as_bytes());
}

fn push_bulk(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(format!("${}\r\n", data.len()).as_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(msg: Message) -> Message {
        let bytes = encode(&msg);
        let (decoded, used) = decode(&bytes).expect("decode");
        assert_eq!(used, bytes.len(), "decode must consume the whole frame");
        decoded
    }

    #[test]
    fn hb_round_trip() {
        let msg = Message::Hb {
            epoch: 42,
            node_id: "primary-east".to_string(),
            role: Role::Primary,
            repl_offset: 1_234_567,
        };
        match round_trip(msg) {
            Message::Hb { epoch, node_id, role, repl_offset } => {
                assert_eq!(epoch, 42);
                assert_eq!(node_id, "primary-east");
                assert_eq!(role, Role::Primary);
                assert_eq!(repl_offset, 1_234_567);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn offer_round_trip() {
        let msg = Message::Offer {
            new_epoch: 7,
            candidate_id: "replica-1".to_string(),
            repl_offset: 99,
        };
        match round_trip(msg) {
            Message::Offer { new_epoch, candidate_id, repl_offset } => {
                assert_eq!(new_epoch, 7);
                assert_eq!(candidate_id, "replica-1");
                assert_eq!(repl_offset, 99);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn accept_round_trip() {
        let msg = Message::Accept {
            epoch: 7,
            accepter_id: "replica-2".to_string(),
        };
        match round_trip(msg) {
            Message::Accept { epoch, accepter_id } => {
                assert_eq!(epoch, 7);
                assert_eq!(accepter_id, "replica-2");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn announce_round_trip() {
        let msg = Message::Announce {
            epoch: 7,
            new_primary_id: "replica-1".to_string(),
            new_primary_addr: "10.0.0.42:6004".to_string(),
        };
        match round_trip(msg) {
            Message::Announce { epoch, new_primary_id, new_primary_addr } => {
                assert_eq!(epoch, 7);
                assert_eq!(new_primary_id, "replica-1");
                assert_eq!(new_primary_addr, "10.0.0.42:6004");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn decode_truncated_returns_more() {
        // Half a frame — decoder must surface Truncated so the
        // caller reads more bytes from the socket.
        let full = encode(&Message::Accept {
            epoch: 1,
            accepter_id: "x".to_string(),
        });
        let half = &full[..full.len() / 2];
        assert!(matches!(decode(half), Err(DecodeError::Truncated)));
    }

    #[test]
    fn decode_unknown_verb_errs() {
        // Valid RESP, unknown verb.
        let bytes = b"*2\r\n$4\r\nPING\r\n$2\r\nok\r\n";
        assert!(matches!(decode(bytes), Err(DecodeError::WrongShape)));
    }

    #[test]
    fn decode_hb_wrong_arity_errs() {
        // `HB` with only 3 args instead of the required 5.
        let bytes = b"*3\r\n$2\r\nHB\r\n$1\r\n1\r\n$4\r\nnode\r\n";
        assert!(matches!(decode(bytes), Err(DecodeError::WrongShape)));
    }

    #[test]
    fn decode_hb_bad_role_errs() {
        // `HB` with a role that's not primary/replica/candidate.
        let mut out = Vec::new();
        push_bulk_array(&mut out, 5);
        push_bulk(&mut out, b"HB");
        push_bulk(&mut out, b"1");
        push_bulk(&mut out, b"node-x");
        push_bulk(&mut out, b"leader");
        push_bulk(&mut out, b"0");
        assert!(matches!(decode(&out), Err(DecodeError::BadRole)));
    }

    #[test]
    fn decode_hb_bad_numeric_errs() {
        let mut out = Vec::new();
        push_bulk_array(&mut out, 5);
        push_bulk(&mut out, b"HB");
        push_bulk(&mut out, b"NaN");
        push_bulk(&mut out, b"node-x");
        push_bulk(&mut out, b"primary");
        push_bulk(&mut out, b"0");
        assert!(matches!(decode(&out), Err(DecodeError::BadNumeric)));
    }

    #[test]
    fn verbs_are_case_insensitive_on_decode() {
        let mut out = Vec::new();
        push_bulk_array(&mut out, 5);
        push_bulk(&mut out, b"hb"); // lowercase
        push_bulk(&mut out, b"1");
        push_bulk(&mut out, b"node-x");
        push_bulk(&mut out, b"primary");
        push_bulk(&mut out, b"0");
        let (msg, _) = decode(&out).expect("decode");
        assert!(matches!(msg, Message::Hb { .. }));
    }
}

use super::*;
use kevy_resp::Argv;

#[test]
fn encoded_replicate_from_matches_what_primary_parses() {
    // Round-trip: encode here, parse via the primary-side parser.
    let bytes = encode_replicate_from(FeedPosition::new(3, 42), "replica-a");
    let mut argv = Argv::default();
    let consumed =
        kevy_resp::parse_command_into(&bytes, &mut argv).expect("parse ok").expect("complete");
    assert_eq!(consumed, bytes.len());
    let req = crate::handshake::HandshakeReq::parse(&argv).expect("handshake ok");
    assert_eq!(req.from, FeedPosition::new(3, 42));
    assert_eq!(req.replica_id, "replica-a");
}

#[test]
fn ack_line_parses_gen_and_offset() {
    assert_eq!(parse_ack_line(b"+ACK 1 0\r\n").unwrap(), FeedPosition::new(1, 0));
    assert_eq!(parse_ack_line(b"+ACK 7 42\r\n").unwrap(), FeedPosition::new(7, 42));
    assert_eq!(parse_ack_line(b"+ACK 2 12345678\r\n").unwrap(), FeedPosition::new(2, 12_345_678));
}

#[test]
fn ack_line_rejects_malformed() {
    assert!(matches!(parse_ack_line(b"+PONG\r\n"), Err(ReplicaError::AckMalformed)));
    assert!(matches!(parse_ack_line(b"+ACK abc 1\r\n"), Err(ReplicaError::AckMalformed)));
    assert!(matches!(parse_ack_line(b"-ERR nope\r\n"), Err(ReplicaError::AckMalformed)));
    // The legacy one-number (pre-4.0) ACK — clean wire break.
    assert!(matches!(parse_ack_line(b"+ACK 42\r\n"), Err(ReplicaError::AckMalformed)));
    // Missing CRLF.
    assert!(matches!(parse_ack_line(b"+ACK 1 1"), Err(ReplicaError::AckMalformed)));
}

#[test]
fn ack_line_rejects_offset_overflow() {
    // 21+ digits — beyond u64::MAX. parse::<u64>() returns Err →
    // AckMalformed.
    assert!(matches!(
        parse_ack_line(b"+ACK 1 99999999999999999999999\r\n"),
        Err(ReplicaError::AckMalformed)
    ));
}

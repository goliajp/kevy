use super::*;

#[test]
fn an_encrypted_client_refuses_a_redirect_it_has_no_key_for() {
    // a node that accepts and never answers: the refusal must come
    // before any request is sent anywhere
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("kevy://{}", l.local_addr().unwrap());
    let mut c = ReadWriteClient::connect_urls(&url, &[&url]).unwrap();
    assert!(!c.encrypted, "plain URLs are not encrypted");
    assert_eq!(c.replica_count(), 1);
    c.encrypted = true;
    // nothing listens at port 1: following the redirect would fail
    // with ConnectionRefused, not PermissionDenied
    let e = c.request_via_writer("127.0.0.1:1", &[b"SET".to_vec()]).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    assert!(e.to_string().contains("no key"), "{e}");
    drop(l);
}

/// The client sends to the primary exactly what the server counts as a
/// write, plus the commands that write no key but only the primary can
/// answer for: checked over every verb the server documents.
#[test]
fn routes_to_the_primary_what_the_server_counts_as_a_write() {
    use kevy_rt::Commands;
    const PRIMARY_ONLY: &[&str] = &[
        "BGREWRITEAOF",
        "BGSAVE",
        "CONFIG",
        "DISCARD",
        "EXEC",
        "MULTI",
        "PUBLISH",
        "REPLICAOF",
        "SAVE",
        "SLAVEOF",
        "UNWATCH",
        "WATCH",
    ];
    let server = kevy::KevyCommands::new();
    let mut differ = Vec::new();
    for m in kevy::verb_meta::VERB_META {
        let server_write = server.is_write(&kevy::Argv::from(vec![m.name.as_bytes().to_vec()]));
        let want = server_write || PRIMARY_ONLY.contains(&m.name);
        if is_write_verb(m.name.as_bytes()) != want {
            differ.push(format!("{} (server write: {server_write})", m.name));
        }
    }
    assert!(kevy::verb_meta::VERB_META.len() > 150, "the server's verb table did not load");
    assert!(differ.is_empty(), "the client routes these unlike the server: {differ:#?}");
}

#[test]
fn writes_classified_correctly() {
    for verb in [&b"SET"[..], b"DEL", b"LPUSH", b"HSET", b"ZADD", b"XADD", b"FLUSHDB", b"REPLICAOF"]
    {
        assert!(is_write_verb(verb), "{:?} should be write", std::str::from_utf8(verb));
    }
}

#[test]
fn reads_classified_correctly() {
    for verb in
        [&b"GET"[..], b"HGET", b"LRANGE", b"SMEMBERS", b"ZSCORE", b"XRANGE", b"PING", b"INFO"]
    {
        assert!(!is_write_verb(verb), "{:?} should be read", std::str::from_utf8(verb));
    }
}

#[test]
fn classification_is_case_insensitive() {
    assert!(is_write_verb(b"set"));
    assert!(is_write_verb(b"Set"));
    assert!(is_write_verb(b"SET"));
    assert!(!is_write_verb(b"get"));
    assert!(!is_write_verb(b"Get"));
}

#[test]
fn long_verb_doesnt_panic_on_classification() {
    // Verbs longer than 32 bytes (silly but legal RESP) are
    // truncated by the upper-buf — they fall through to the
    // catch-all read classification.
    assert!(!is_write_verb(&[b'X'; 64]));
}

// ---- scope MISDIRECTED parser ----

#[test]
fn parse_misdirected_basic() {
    let r = Reply::Error(b"MISDIRECTED writer is 10.0.0.1:6004".to_vec());
    assert_eq!(parse_misdirected(&r).as_deref(), Some("10.0.0.1:6004"));
}

#[test]
fn parse_misdirected_strips_trailing_crlf() {
    // Some encoders leave `\r\n` in the Error payload; parser
    // tolerates both shapes.
    let r = Reply::Error(b"MISDIRECTED writer is 10.0.0.1:6004\r\n".to_vec());
    assert_eq!(parse_misdirected(&r).as_deref(), Some("10.0.0.1:6004"));
}

#[test]
fn parse_misdirected_rejects_unrelated_error() {
    let r = Reply::Error(b"ERR something else".to_vec());
    assert!(parse_misdirected(&r).is_none());
    // Non-Error replies are also rejected.
    let r = Reply::Simple(b"OK".to_vec());
    assert!(parse_misdirected(&r).is_none());
}

#[test]
fn split_host_port_dotted_v4_and_dns() {
    assert_eq!(split_host_port("10.0.0.1:6004"), Some(("10.0.0.1", 6004)));
    assert_eq!(split_host_port("db.local:6105"), Some(("db.local", 6105)));
}

#[test]
fn parse_quiesced_basic() {
    let r = Reply::Error(b"QUIESCED migrating to 10.0.0.1:6004".to_vec());
    assert_eq!(parse_quiesced(&r).as_deref(), Some("10.0.0.1:6004"));
}

#[test]
fn parse_quiesced_strips_trailing_crlf() {
    let r = Reply::Error(b"QUIESCED migrating to 10.0.0.1:6004\r\n".to_vec());
    assert_eq!(parse_quiesced(&r).as_deref(), Some("10.0.0.1:6004"));
}

#[test]
fn parse_quiesced_rejects_unrelated_error() {
    let r = Reply::Error(b"MISDIRECTED writer is 10.0.0.1:6004".to_vec());
    assert!(parse_quiesced(&r).is_none());
    let r = Reply::Simple(b"OK".to_vec());
    assert!(parse_quiesced(&r).is_none());
}

#[test]
fn split_host_port_rejects_bad_inputs() {
    assert!(split_host_port("nohost:").is_none());
    assert!(split_host_port(":6004").is_none());
    assert!(split_host_port("no-colon").is_none());
    assert!(split_host_port("host:99999").is_none()); // u16 overflow
}

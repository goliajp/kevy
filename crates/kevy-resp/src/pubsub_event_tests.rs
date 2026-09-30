//! Tests for `pubsub_event`, kept apart to hold the event file under 500 lines.
use super::*;

/// Every missing field names its own verb and its own field.
///
/// `PubsubEvent::try_from` is six near-identical arms, each repeating the
/// verb's name in two or three error strings. That shape has one
/// characteristic defect — an arm copied from the one above it and not
/// fully renamed — and the error text is the only place it would show.
/// A `psubscribe` frame reporting "subscribe: missing channel" sends the
/// reader to the wrong arm, and nothing else in the type system or the
/// tests would notice.
///
/// Table-driven over every arm and every truncation point, which is also
/// what covers the eighteen never-executed regions the coverage atlas
/// reports here: they are all the refusal side of `it.next()`.
#[test]
fn every_missing_field_names_its_own_verb_and_field() {
    // (verb, the fields it consumes in order)
    let arms: &[(&str, &[&str])] = &[
        ("subscribe", &["channel", "count"]),
        ("psubscribe", &["pattern", "count"]),
        ("unsubscribe", &["channel", "count"]),
        ("punsubscribe", &["pattern", "count"]),
        ("message", &["channel", "payload"]),
        ("pmessage", &["pattern", "channel", "payload"]),
    ];

    let mut checked = 0;
    for (verb, fields) in arms {
        for (n, missing) in fields.iter().enumerate() {
            // The verb, then every field before the missing one, then
            // nothing — so `it.next()` returns `None` exactly there.
            let mut items = vec![Reply::Bulk(verb.as_bytes().to_vec())];
            items.extend((0..n).map(|_| Reply::Bulk(b"x".to_vec())));
            let err = PubsubEvent::try_from(Reply::Array(items))
                .expect_err("{verb} with {n} fields must not classify")
                .to_string();
            let want = format!("{verb}: missing {missing}");
            assert!(
                err.contains(&want),
                "a {verb} frame missing its {missing} said {err:?}, not {want:?}"
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 13, "every arm and truncation point was exercised");
}

#[test]
fn classify_subscribe_ack() {
    let r = Reply::Array(vec![
        Reply::Bulk(b"subscribe".to_vec()),
        Reply::Bulk(b"chan".to_vec()),
        Reply::Int(1),
    ]);
    assert_eq!(
        PubsubEvent::try_from(r).unwrap(),
        PubsubEvent::Subscribe { channel: b"chan".to_vec(), count: 1 }
    );
}

#[test]
fn classify_message_event() {
    let r = Reply::Array(vec![
        Reply::Bulk(b"message".to_vec()),
        Reply::Bulk(b"news".to_vec()),
        Reply::Bulk(b"hello".to_vec()),
    ]);
    assert_eq!(
        PubsubEvent::try_from(r).unwrap(),
        PubsubEvent::Message { channel: b"news".to_vec(), payload: b"hello".to_vec() }
    );
}

#[test]
fn classify_pmessage_event() {
    let r = Reply::Array(vec![
        Reply::Bulk(b"pmessage".to_vec()),
        Reply::Bulk(b"news.*".to_vec()),
        Reply::Bulk(b"news.tech".to_vec()),
        Reply::Bulk(b"hi".to_vec()),
    ]);
    assert_eq!(
        PubsubEvent::try_from(r).unwrap(),
        PubsubEvent::Pmessage {
            pattern: b"news.*".to_vec(),
            channel: b"news.tech".to_vec(),
            payload: b"hi".to_vec(),
        }
    );
}

#[test]
fn classify_unsubscribe_with_nil_channel() {
    let r = Reply::Array(vec![Reply::Bulk(b"unsubscribe".to_vec()), Reply::Nil, Reply::Int(0)]);
    assert_eq!(
        PubsubEvent::try_from(r).unwrap(),
        PubsubEvent::Unsubscribe { channel: None, count: 0 }
    );
}

#[test]
fn classify_accepts_push_frame() {
    // RESP3 servers wrap the same shape in a `>N` push frame.
    let r = Reply::Push(vec![
        Reply::Bulk(b"message".to_vec()),
        Reply::Bulk(b"c".to_vec()),
        Reply::Bulk(b"p".to_vec()),
    ]);
    assert_eq!(
        PubsubEvent::try_from(r).unwrap(),
        PubsubEvent::Message { channel: b"c".to_vec(), payload: b"p".to_vec() }
    );
}

#[test]
fn classify_accepts_simple_string_fields() {
    // `take_bulk` accepts `Simple` as well as `Bulk` — a server may
    // send the kind/channel as simple strings.
    let r = Reply::Array(vec![
        Reply::Simple(b"subscribe".to_vec()),
        Reply::Simple(b"chan".to_vec()),
        Reply::Int(2),
    ]);
    assert_eq!(
        PubsubEvent::try_from(r).unwrap(),
        PubsubEvent::Subscribe { channel: b"chan".to_vec(), count: 2 }
    );
}

#[test]
fn classify_rejects_unknown_kind() {
    let r = Reply::Array(vec![
        Reply::Bulk(b"bogus".to_vec()),
        Reply::Bulk(b"x".to_vec()),
        Reply::Int(0),
    ]);
    assert!(PubsubEvent::try_from(r).is_err());
}

#[test]
fn classify_rejects_wrong_arity() {
    let r = Reply::Array(vec![Reply::Bulk(b"subscribe".to_vec()), Reply::Bulk(b"x".to_vec())]);
    assert!(PubsubEvent::try_from(r).is_err());
}

//! The IK vectors from two independent implementations (cacophony, snow),
//! byte for byte, and the ways a handshake must refuse.

use crate::{Error, Frames, Initiator, Keypair, Responder, Transport, frame};

fn hex(s: &str) -> Vec<u8> {
    if s == "-" {
        return Vec::new();
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn key(s: &str) -> [u8; 32] {
    hex(s).try_into().unwrap()
}

struct Vector {
    fields: std::collections::HashMap<String, String>,
    messages: Vec<(Vec<u8>, Vec<u8>)>,
}

fn load(fixture: &str) -> Vector {
    let mut v = Vector { fields: Default::default(), messages: Vec::new() };
    for line in fixture.lines().filter(|l| !l.starts_with('#')) {
        let parts: Vec<&str> = line.split(' ').collect();
        match parts[0] {
            "message" => v.messages.push((hex(parts[1]), hex(parts[2]))),
            k => {
                v.fields.insert(k.to_string(), parts[1].to_string());
            }
        }
    }
    v
}

fn run_vector(fixture: &str) -> usize {
    let v = load(fixture);
    let f = |k: &str| v.fields[k].as_str();
    assert_eq!(f("init_prologue"), f("resp_prologue"));
    let prologue = hex(f("init_prologue"));
    let (i_s, i_e) = (
        Keypair::from_secret(key(f("init_static"))),
        Keypair::from_secret(key(f("init_ephemeral"))),
    );
    let (r_s, r_e) = (
        Keypair::from_secret(key(f("resp_static"))),
        Keypair::from_secret(key(f("resp_ephemeral"))),
    );
    assert_eq!(r_s.public(), key(f("init_remote_static")), "fixture pairs the right keys");

    let (p0, c0) = &v.messages[0];
    let (m1, init) = Initiator::start(&i_s, &r_s.public(), i_e, &prologue, p0).unwrap();
    assert_eq!(&m1, c0, "message 0");
    let (got, resp) = Responder::accept(&r_s, r_e, &prologue, &m1).unwrap();
    assert_eq!(&got, p0);
    assert_eq!(resp.remote_static(), i_s.public());

    let (p1, c1) = &v.messages[1];
    let (m2, mut rt) = resp.finish(p1).unwrap();
    assert_eq!(&m2, c1, "message 1");
    let (got, mut it) = init.finish(&m2).unwrap();
    assert_eq!(&got, p1);
    if let Some(h) = v.fields.get("handshake_hash") {
        assert_eq!(it.handshake_hash(), key(h));
        assert_eq!(rt.handshake_hash(), key(h));
    }

    for (i, (p, c)) in v.messages.iter().enumerate().skip(2) {
        let (tx, rx): (&mut Transport, &mut Transport) =
            if i % 2 == 0 { (&mut it, &mut rt) } else { (&mut rt, &mut it) };
        let sealed = tx.seal(p).unwrap();
        assert_eq!(&sealed, c, "message {i}");
        assert_eq!(&rx.open(&sealed).unwrap(), p);
    }
    v.messages.len()
}

#[test]
fn cacophony_ik_vector() {
    assert_eq!(run_vector(include_str!("../tests/data/cacophony_ik.txt")), 6);
}

#[test]
fn snow_ik_vector() {
    assert_eq!(run_vector(include_str!("../tests/data/snow_ik.txt")), 4);
}

fn pair() -> (Keypair, Keypair) {
    (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32]))
}

fn first_message() -> (Vec<u8>, Initiator) {
    let (server, client) = pair();
    Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"p", b"payload")
        .unwrap()
}

#[test]
fn every_altered_byte_of_the_first_message_is_refused() {
    let (server, _) = pair();
    let (m1, _) = first_message();
    for i in 0..m1.len() {
        let mut bad = m1.clone();
        bad[i] ^= 0x01;
        let r = Responder::accept(&server, Keypair::from_secret([4; 32]), b"p", &bad);
        assert!(r.is_err(), "byte {i} altered and still accepted");
    }
}

#[test]
fn every_altered_byte_of_the_reply_is_refused() {
    let (server, _) = pair();
    let (m1, init) = first_message();
    let (_, resp) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"p", &m1).unwrap();
    let (m2, _) = resp.finish(b"reply").unwrap();
    for i in 0..m2.len() {
        let mut bad = m2.clone();
        bad[i] ^= 0x80;
        let (m1b, init_b) = first_message();
        assert_eq!(m1b, m1);
        assert!(init_b.finish(&bad).is_err(), "byte {i} altered and still accepted");
    }
    assert!(init.finish(&m2).is_ok());
}

#[test]
fn a_different_prologue_or_server_key_fails_the_handshake() {
    let (server, _) = pair();
    let (m1, _) = first_message();
    let other = Keypair::from_secret([9; 32]);
    assert_eq!(
        Responder::accept(&server, Keypair::from_secret([4; 32]), b"q", &m1).err(),
        Some(Error::Decrypt)
    );
    assert_eq!(
        Responder::accept(&other, Keypair::from_secret([4; 32]), b"p", &m1).err(),
        Some(Error::Decrypt)
    );
}

#[test]
fn a_small_order_ephemeral_is_refused() {
    let (server, _) = pair();
    let mut m1 = first_message().0;
    m1[..32].fill(0);
    assert_eq!(
        Responder::accept(&server, Keypair::from_secret([4; 32]), b"p", &m1).err(),
        Some(Error::LowOrderPoint)
    );
}

#[test]
fn truncated_messages_are_refused() {
    let (server, _) = pair();
    let (m1, init) = first_message();
    for n in [0, 31, 32, 79, 95] {
        assert!(
            Responder::accept(&server, Keypair::from_secret([4; 32]), b"p", &m1[..n]).is_err(),
            "length {n}"
        );
    }
    assert_eq!(init.finish(&[0; 31]).err(), Some(Error::Truncated));
}

#[test]
fn transport_refuses_replay_reorder_and_tampering() {
    let (server, _) = pair();
    let (m1, init) = first_message();
    let (_, resp) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"p", &m1).unwrap();
    let (m2, mut s) = resp.finish(b"").unwrap();
    let (_, mut c) = init.finish(&m2).unwrap();
    let (a, b) = (c.seal(b"first").unwrap(), c.seal(b"second").unwrap());
    assert_eq!(s.open(&b), Err(Error::Decrypt), "out of order");
    assert_eq!(s.open(&a).unwrap(), b"first", "a failed open does not advance the nonce");
    assert_eq!(s.open(&a), Err(Error::Decrypt), "replay");
    let mut t = b.clone();
    t[0] ^= 1;
    assert_eq!(s.open(&t), Err(Error::Decrypt), "tampered");
    assert_eq!(s.open(&b).unwrap(), b"second");
    assert_eq!(s.open(&[0; 15]), Err(Error::Truncated));
    let back = s.seal(b"reply").unwrap();
    assert_eq!(c.open(&back).unwrap(), b"reply", "the other direction has its own key");
}

#[test]
fn frames_reassemble_from_any_split() {
    let msgs: Vec<Vec<u8>> = (0..20).map(|i| vec![i as u8; i * 37]).collect();
    let stream: Vec<u8> = msgs.iter().flat_map(|m| frame(m).unwrap()).collect();
    for step in [1, 2, 3, 7, 64, 1000, stream.len()] {
        let mut f = Frames::default();
        let mut out = Vec::new();
        for piece in stream.chunks(step) {
            f.push(piece);
            while let Some(m) = f.next() {
                out.push(m);
            }
        }
        assert_eq!(out, msgs, "pieces of {step}");
    }
    assert_eq!(frame(&vec![0; 65_536]).err(), Some(Error::TooLong));
}

#[test]
fn errors_describe_themselves() {
    for e in [
        Error::Decrypt,
        Error::Truncated,
        Error::LowOrderPoint,
        Error::TooLong,
        Error::NonceExhausted,
    ] {
        assert!(!e.to_string().is_empty());
    }
}

#[test]
fn a_message_longer_than_noise_allows_is_refused_before_sealing() {
    let (server, client) = pair();
    let (m1, init) =
        Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"", b"")
            .unwrap();
    let (_, r) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"", &m1).unwrap();
    let (m2, _) = r.finish(b"").unwrap();
    let (_, mut c) = init.finish(&m2).unwrap();
    assert!(c.seal(&vec![0; crate::MAX_MESSAGE - 16]).is_ok());
    assert_eq!(c.seal(&vec![0; crate::MAX_MESSAGE - 15]).err(), Some(Error::TooLong));
}

#[test]
fn a_key_pair_prints_its_public_half_only() {
    let kp = Keypair::from_secret([7; 32]);
    let shown = format!("{kp:?}");
    assert!(shown.contains(&format!("{:?}", kp.public())));
    assert!(!shown.contains(&format!("{:?}", [7u8; 32])));
}

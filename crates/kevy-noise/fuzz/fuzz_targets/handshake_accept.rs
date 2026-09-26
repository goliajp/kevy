//! A responder fed arbitrary first messages must refuse them without
//! panicking, and a well-formed one with any bytes flipped must not pass.

#![no_main]

use kevy_noise::{Initiator, Keypair, Responder};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let server = Keypair::from_secret([1; 32]);
    let _ = Responder::accept(&server, Keypair::from_secret([4; 32]), b"fuzz", data);

    // a genuine first message, xored with the input: accepted only when
    // the input changed nothing
    let client = Keypair::from_secret([2; 32]);
    let (m1, _) = Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"fuzz", b"payload")
        .expect("a well-formed handshake");
    let mut bad = m1.clone();
    for (b, d) in bad.iter_mut().zip(data) {
        *b ^= d;
    }
    let accepted = Responder::accept(&server, Keypair::from_secret([4; 32]), b"fuzz", &bad).is_ok();
    assert_eq!(accepted, bad == m1);
});

//! Arbitrary bytes pushed through the frame reassembler in arbitrary
//! pieces, each frame handed to a live transport: nothing may panic, and
//! nothing the attacker made up may decrypt.

#![no_main]

use kevy_noise::{Frames, Initiator, Keypair, Responder};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (server, client) = (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32]));
    let (m1, init) = Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"", b"").unwrap();
    let (_, resp) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"", &m1).unwrap();
    let (m2, mut s) = resp.finish(b"").unwrap();
    let _ = init.finish(&m2).unwrap();

    let Some((&step, rest)) = data.split_first() else { return };
    let mut frames = Frames::default();
    for piece in rest.chunks(usize::from(step).max(1)) {
        frames.push(piece);
        while let Some(msg) = frames.next() {
            assert!(s.open(&msg).is_err(), "forged message decrypted");
        }
    }
});

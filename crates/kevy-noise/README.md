# kevy-noise

The Noise IK handshake, `Noise_IK_25519_ChaChaPoly_BLAKE2s`, and the
transport it establishes — without I/O, on the primitives in
[kevy-crypto](../kevy-crypto).

- The initiator knows the responder's static public key in advance; the
  responder learns the initiator's static key from the first message and can
  refuse it there. One round trip.
- Ephemeral keys are passed in, so the crate never touches an entropy
  source; each handshake needs fresh ones from the operating system.
- Messages travel length-prefixed (`frame`, `Frames`) on a stream the caller
  owns; `Transport` seals and opens them in order and refuses replays,
  reordering and tampering.
- A peer key of small order is refused.

```rust
use kevy_noise::{Initiator, Keypair, Responder};

let server = Keypair::from_secret([1; 32]);
let client = Keypair::from_secret([2; 32]);

let (m1, init) = Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"app", b"").unwrap();
let (_, resp) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"app", &m1).unwrap();
// decide here, from resp.remote_static(), whether this client may connect
let (m2, mut server_side) = resp.finish(b"").unwrap();
let (_, mut client_side) = init.finish(&m2).unwrap();

let msg = client_side.seal(b"hello").unwrap();
assert_eq!(server_side.open(&msg).unwrap(), b"hello");
```

## How it is checked

- the `Noise_IK_25519_ChaChaPoly_BLAKE2s` vectors of two independent
  implementations, cacophony and snow, byte for byte, including the
  handshake hash and the transport messages that follow;
- compared byte for byte against snow on random keys, prologues and
  payloads, handshake and transport in both directions;
- every byte of both handshake messages altered in turn and refused;
  replay, reordering, truncation and small-order keys refused;
- fuzzed: the responder's handshake parser, and the frame reassembler
  feeding a live transport.

The test vectors under `tests/data/` are converted from their published
sources; each file's header names the source and its SHA-256.

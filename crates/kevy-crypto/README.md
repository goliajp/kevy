# kevy-crypto

The primitives of the Noise pattern family `Noise_*_25519_ChaChaPoly_BLAKE2s`,
in pure Rust with no dependencies and no `unsafe`:

| Module | What | Specification |
|---|---|---|
| `aead` | ChaCha20-Poly1305, in place | RFC 8439 |
| `x25519` | Diffie-Hellman over Curve25519 | RFC 7748 |
| `blake2s` | BLAKE2s, unkeyed and keyed, 1–32 byte digests | RFC 7693 |
| `hkdf` | HMAC-BLAKE2s and Noise's HKDF | RFC 2104, Noise rev. 34 §4.3 |

`ct_eq` compares byte strings in time that depends only on their lengths.

```rust
use kevy_crypto::{aead, x25519};

let (a, b) = ([3u8; 32], [5u8; 32]);
let shared_a = x25519::x25519(&a, &x25519::x25519(&b, &x25519::BASEPOINT));
let shared_b = x25519::x25519(&b, &x25519::x25519(&a, &x25519::BASEPOINT));
assert_eq!(shared_a, shared_b);

let key = kevy_crypto::blake2s::hash(&shared_a);
let mut msg = *b"hello";
let tag = aead::seal(&key, &[0; 12], b"", &mut msg);
aead::open(&key, &[0; 12], b"", &mut msg, &tag).unwrap();
```

## How it is checked

- every vector in RFC 8439 Appendix A, RFC 7748 §5.2 and §6.1 (including
  the 1,000-iteration test), and RFC 7693's `"abc"` and self-test;
- Wycheproof's ChaCha20-Poly1305 and X25519 suites, and the 256 keyed
  BLAKE2s answers from the BLAKE2 reference repository;
- compared byte for byte against RustCrypto on random inputs;
- statistical timing checks of the secret-dependent paths on x86_64 and
  aarch64.

Secrets only steer data, never branches or memory indices. A compiler does
not guarantee that this survives optimisation, and the crate has not been
audited by a third party; weigh that before using it where a standard TLS
library is an option.

The test vectors under `tests/data/` are converted from their published
sources; each file's header names the source and its SHA-256. Wycheproof is
Apache-2.0; the BLAKE2 test vectors are CC0.

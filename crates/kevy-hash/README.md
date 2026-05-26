# kevy-hash

A non-cryptographic hash (`FxFmix`) for **single-trust-domain** keyspaces —
pure Rust, zero dependencies.

`FxFmix` is FxHash's mixing function followed by an `fmix64` finalizer (the
finalizer from MurmurHash3). The combination gets us avalanche on the low bits
that FxHash alone misses, while staying 3.7–7× faster than `std::collections`'s
SipHash on short keys.

Part of [kevy](https://crates.io/crates/kevy) — kevy is a single-process server
with no untrusted clients within a shard, so DoS-resistant SipHash is
unnecessary overhead.

```rust
use kevy_hash::FxFmixHasher;
use std::hash::Hasher;

let mut h = FxFmixHasher::default();
h.write(b"foo");
let hash = h.finish();
```

For drop-in `HashMap` use:

```rust
use kevy_hash::FxFmixBuildHasher;
use std::collections::HashMap;

let mut m: HashMap<&str, i32, FxFmixBuildHasher> = HashMap::default();
m.insert("x", 1);
```

## Trust model

⚠️ `FxFmix` is **not** DoS-resistant. Do not feed it adversary-controlled
keys without rate-limiting or per-shard isolation first. For kevy this is
fine — one shard = one trust domain.

## License

MIT OR Apache-2.0

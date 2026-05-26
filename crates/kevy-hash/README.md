# kevy-hash

A non-cryptographic hash (`FxHasher` + `fmix64`) for **single-trust-domain**
keyspaces — pure Rust, zero dependencies.

`FxHasher` is FxHash's mixing function followed by an `fmix64` finalizer (the
finalizer from MurmurHash3). The combination gets avalanche on the low bits
that FxHash alone misses, while staying 3.7–7× faster than `std::collections`'s
SipHash on short keys.

Part of [kevy](https://crates.io/crates/kevy) — kevy is a single-process server
with no untrusted clients within a shard, so DoS-resistant SipHash is
unnecessary overhead.

```rust
use kevy_hash::FxHasher;
use std::hash::Hasher;

let mut h = FxHasher::default();
h.write(b"foo");
let hash: u64 = h.finish();
```

For drop-in `HashMap` use, the crate provides a `BuildHasher` + ready-made
type aliases:

```rust
use kevy_hash::{FxBuildHasher, FxHashMap};

let mut m: FxHashMap<&str, i32> = FxHashMap::default();
m.insert("x", 1);

// equivalent, more explicit:
use std::collections::HashMap;
let mut m2: HashMap<&str, i32, FxBuildHasher> = HashMap::default();
m2.insert("y", 2);
```

For callers that don't need the `Hasher` state machine, the `KevyHash`
trait gives a stateless one-shot:

```rust
use kevy_hash::KevyHash;
let h: u64 = b"hello"[..].kevy_hash();
```

## Trust model

⚠️ `FxHasher` is **not** DoS-resistant. Do not feed it adversary-controlled
keys without rate-limiting or per-shard isolation first. For kevy this is
fine — one shard = one trust domain.

## License

MIT OR Apache-2.0

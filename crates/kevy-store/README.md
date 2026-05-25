# kevy-store

The keyspace for [kevy](https://crates.io/crates/kevy) — a zero-dependency,
pure-Rust, single-threaded multi-type store with lazy expiration.

Each Redis data type is backed by a **modern `std` structure**, not Redis's
legacy encodings:

| Type | Backing structure |
|------|-------------------|
| String | `Vec<u8>` |
| Hash / Set | `HashMap` / `HashSet` (hashbrown Swiss table) |
| List | `VecDeque` (ring buffer — O(1) at both ends) |
| Sorted set | `HashMap` + `BTreeSet<(score, member)>` (a B-tree, not a skiplist) |

- Lazy TTL expiry; `WRONGTYPE` errors via [`StoreError`].
- `&mut self`, lock-free API — designed to be sharded one-per-core.
- Snapshot hooks (`snapshot_each` / `load_*`) for persistence.
- `#![forbid(unsafe_code)]`, zero dependencies.

```rust
use kevy_store::Store;

let mut s = Store::new();
s.set(b"k", b"v".to_vec(), None, false, false);
assert_eq!(s.get(b"k").unwrap(), Some(&b"v"[..]));
```

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE) at your option.

# kevy-persist

Durability for a [kevy-store](https://crates.io/crates/kevy-store) `Store` —
zero-dependency, pure Rust over `std::fs`. Part of
[kevy](https://crates.io/crates/kevy).

Two mechanisms:

- **Snapshot (RDB-style)** — `save_snapshot` dumps a whole store to a temp file,
  fsyncs, then atomically renames; `load_snapshot` restores it. Compact,
  type-tagged binary format covering every value type.
- **AOF (append-only file)** — an `Aof` command log with `Always` / `EverySec` /
  `No` fsync policies; `replay_aof` re-applies it on startup and tolerates a
  truncated trailing frame from a crash mid-write.

Paired (snapshot + AOF), `SAVE` truncates the AOF so a reload of `snapshot @ T0`
plus the AOF of writes since `T0` never double-applies. `#![forbid(unsafe_code)]`.

```rust
use kevy_persist::{Aof, Fsync, replay_aof};

let path = std::env::temp_dir().join("example.aof");
let mut aof = Aof::open(&path, Fsync::No).unwrap();
aof.append(&[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]).unwrap();
drop(aof);

let mut cmds = Vec::new();
replay_aof(&path, |args| cmds.push(args.to_vec())).unwrap();
assert_eq!(cmds.len(), 1);
# std::fs::remove_file(&path).ok();
```

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE) at your option.

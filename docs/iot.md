# kevy on IoT devices

`kevy-embedded` scales down as deliberately as the server scales up:
a feature-tiered build that goes from a ~655 KB in-memory KV core to
the full index/replication surface, musl cross-builds for Linux-class
IoT boards (Pi Zero 2, OpenWrt routers, industrial ARM), and a
`no_std` core proven on a bare-metal Cortex-M target. Resource
budgets are enforced by a gate, not promised by a README.

## The feature tiers

`kevy-embedded` exposes one Cargo feature per subsystem. The default
is everything; IoT builds start from `core` and add only what the
device actually does:

| Feature | Adds | Pulls in |
|---|---|---|
| `core` | KV + TTL + counters + pub/sub + atomic/pipeline facades | (the base surface — always on in practice; named so the minimal archetype is spellable) |
| `persist` | Snapshot + AOF durability: `Config::with_persist`, `save_snapshot`, `rewrite_aof`, AOF replay on open | `kevy-persist` |
| `index` | Declared secondary indexes + materialized views | `kevy-index` |
| `text` | Full-text (BM25) index segments | `index` + `kevy-text` |
| `vector` | ANN / HNSW vector index segments | `index` + `kevy-vector` |
| `replicate` | Embed-as-replica, embed-as-writer, and the CDC feed | `persist` + `kevy-replicate` + `kevy-resp` |
| `listener` | The read-only RESP listener ([docs/embedded-listener.md](embedded-listener.md)) | — |

Dependencies between tiers are encoded in the features themselves:
`text` and `vector` imply `index`; `replicate` implies `persist`
(replicated frames replay through the AOF verb table). Whatever you
leave out is not just dead code that the linker drops — the crates
behind it never enter the build graph at all.

Six archetypes are compile-gated in CI on every push, so each
combination keeps building as the workspace moves:

```text
core                        # sensor cache: RAM-only KV + TTL
core,persist                # + survives power cycles
core,index                  # + declared indexes / views
core,index,text,vector      # + search (BM25, HNSW)
core,persist,replicate      # + edge node feeding a hub
core,listener               # + redis-cli-able diagnostics port
```

In `Cargo.toml`:

```toml
[dependencies]
kevy-embedded = { version = "4", default-features = false, features = ["core"] }
```

The API face is the same at every tier: `Store::open(Config)`,
`KevyResult` / `KevyError` errors, borrowed-slice argv on the write
methods. Code written against `core` recompiles unchanged when the
device later grows `persist`.

## Resource budgets (gated, not aspirational)

Two budget lines, enforced by
[`bench/iotgate.sh`](../bench/iotgate.sh) as a ratchet — raising
either number requires a written verdict:

| Budget | Line | Measured |
|---|---|---|
| Binary size (`core` archetype example, `--profile iot`) | ≤ 700 KB | **655 KB** |
| Empty-store RSS right after `open` (Linux) | ≤ 2 MB | gated on Linux runs |

The `iot` cargo profile is defined in the workspace root — `release`
codegen with size the priority:

```toml
[profile.iot]
inherits = "release"
opt-level = "z"     # size over speed
lto = true          # fat LTO: the linker drops unused subsystems
codegen-units = 1
strip = true
```

Reproduce the size number:

```sh
cargo build --profile iot -p kevy-embedded --example iot_core \
  --no-default-features --features core
ls -l target/iot/examples/iot_core
```

## A sensor-cache in full

The `iot_core` example is the shape most devices need — an in-memory
KV with TTLs and a manually driven expiry sweep (no background
threads unless you ask for them):

```rust
use kevy_embedded::{Config, Store};

fn main() -> kevy_embedded::KevyResult<()> {
    // Manual reaper: no thread is spawned; the device's own loop
    // drives expiry at whatever cadence it already runs.
    let s = Store::open(Config::default().with_ttl_reaper_manual())?;

    s.set(b"sensor:1", b"22.5")?;
    s.set(b"sensor:2", b"3.3")?;
    s.expire(b"sensor:2", core::time::Duration::from_secs(60))?;

    // Call from the main loop / timer ISR bottom half:
    let _expired = s.tick();
    Ok(())
}
```

Leave the reaper on its default (a background thread) on boards where
a thread is cheap; take `with_ttl_reaper_manual()` + `tick()` where
you own the loop. TTL precision tracks the tick cadence.

## Cross-compiling: musl and the CI matrix

Static musl binaries are the deployment currency of Linux-class IoT —
one file, no glibc coupling, `scp` and run. Both musl targets are
compile-gated in CI with the **full** default feature surface:

```sh
rustup target add aarch64-unknown-linux-musl
cargo build --profile iot --target aarch64-unknown-linux-musl -p kevy-embedded

rustup target add armv7-unknown-linux-musleabihf
cargo build --profile iot --target armv7-unknown-linux-musleabihf -p kevy-embedded
```

kevy's zero-dependency law pays off here: there is no C library to
cross-compile and no `-sys` crate zoo — the only OS boundary is
kevy's own `kevy-sys`, and the embedded closure below `core` doesn't
even include that.

Running the test suite under emulation is a one-liner when you want
more than a compile proof:

```sh
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER=qemu-aarch64 \
  cargo test -p kevy-embedded --target aarch64-unknown-linux-musl
```

## The `no_std` core

Below Linux entirely, the storage stones build without `std`. Five
crates carry `#![no_std]` cores behind their `std` default feature —
`kevy-store`, `kevy-hash`, `kevy-bytes`, `kevy-map`, `kevy-madvise` —
and CI proves the combination on a real MCU target
(`thumbv7em-none-eabihf`, Cortex-M4/M7 class):

```sh
cargo check --target thumbv7em-none-eabihf -p kevy-store \
  --no-default-features --features alloc,external-clock
```

What the `no_std` cut means in practice:

- **`alloc` is required** — the store is a heap data structure; you
  provide a `#[global_allocator]`. There is no `alloc`-free tier.
- **`external-clock` replaces the OS clock**: the host feeds time
  through `set_clock_ns` (monotonic) and `set_wall_clock_ms` (wall),
  the same host-fed clock contract the browser build uses
  ([docs/wasm.md](wasm.md)).
- **No threads, no files, no sockets** — persistence, replication,
  and the listener are `std`-tier features by nature; the `no_std`
  core is the in-memory engine.

One detail is worth knowing because it is the kind that silently
breaks elsewhere: the host-fed clock is a single 64-bit cell that
must read atomically. On ISAs with 64-bit atomics it is a plain
`AtomicU64`; on 32-bit-only MCUs (ARMv7E-M has no 64-bit atomics) it
degrades to a **single-writer seqlock over two `AtomicU32` halves** —
readers retry on a torn read, and since the host feeds the clock from
one context, the retry loop settles immediately. Feeding the clock
from multiple contexts concurrently is outside the contract.

## Sizing guidance

- The 655 KB / 2 MB numbers are the `core` archetype on the `iot`
  profile — the floor, not the typical. Each tier you add buys its
  subsystem's code and working memory; if the number matters, measure
  your exact feature set with the `iotgate` recipe above.
- Memory scales with the keyspace: the empty-store RSS budget exists
  precisely so the baseline cost of *having* kevy stays trivial next
  to your data.
- If the device needs a diagnostics port, `listener` gives you a
  read-only RESP endpoint that `redis-cli` can talk to — see
  [docs/embedded-listener.md](embedded-listener.md) — for the cost of
  one thread and one socket, with writes structurally refused.
- Durability on flash: `persist` writes the same AOF/snapshot formats
  as the server ([docs/persistence.md](persistence.md)), so data
  written on the device reads back anywhere in the fleet. AOF fsync
  on SD-card-class storage wants `EverySec` (the default), not
  `Always`.

## What this is not

There is no attempt to make the *server* small: `kevy` (the binary),
`kevy-rt`, `kevy-uring` and friends assume a real kernel and are out
of scope for the IoT cut. The embedded library is the product here —
your firmware is the server.

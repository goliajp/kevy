# kevy on IoT devices

`kevy-embedded` scales down as deliberately as the server scales up:
a feature-tiered build that goes from a ~411 KB in-memory KV core to
the full index/replication surface, musl cross-builds for Linux-class
IoT boards (Pi Zero 2, OpenWrt routers, industrial ARM), and a
`no_std` core that has been booted and exercised on a bare-metal
Cortex-M target, not merely compile-checked. Resource
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
| Binary size (`core` consumer, static musl) | ≤ 600 KB | **454 KB** (x86_64) · **411 KB** (aarch64) · **383 KB** (armv7) |
| Empty-store RSS right after `open` (Linux) | ≤ 2 MB | **336 KB** (aarch64) |

> These are measured on `bench/iot-consumer` — a crate deliberately kept
> **outside** the workspace whose only dependency is `kevy-embedded`. That
> is the shape you ship. An earlier version of this gate sized a workspace
> `--example` instead; building an example pulls dev-dependencies (the
> `kevy` server crate), which inflated the reported size by ~60%.

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
one file, no glibc coupling, `scp` and run.

### Platform coverage — what has actually been run

Every row below was built AND executed, not merely compile-checked. The
`core`-tier consumer was run on a real kernel for that architecture; the
sizes are the static-musl artifact.

| Target | Board class | Size | Status |
|---|---|---|---|
| `x86_64-unknown-linux-musl` | industrial x86, gateways | 454 KB | **run** |
| `aarch64-unknown-linux-musl` | Pi Zero 2 / Pi 4-5 / most ARM64 SBC | 411 KB | **run — natively**, 336 KB empty-store RSS, 2.86M store-ops/s |
| `armv7-unknown-linux-musleabihf` | Pi 2-3, older 32-bit ARM | 383 KB | **run** (emulated) |
| `arm-unknown-linux-musleabihf` | Pi Zero / Pi 1 (ARMv6) | 411 KB | **run** (emulated) |
| `riscv64gc-unknown-linux-musl` | RISC-V SBC | 420 KB | **run** (emulated) — needs a RISC-V cross-gcc as the linker (its target spec wants `libgcc_s`, which `rust-lld` cannot supply) plus `crt-static` |
| `thumbv7em-none-eabihf` | Cortex-M4/M7 MCU | 145 KB firmware | **run on bare metal** — `kevy-store` only, not the full `kevy-embedded` surface (that needs `std`). See below. |

The aarch64 numbers are the trustworthy performance ones: that row ran
natively on ARM64. The armv7/ARMv6 rows prove the code is correct on
32-bit ARM, but their timing and RSS carry emulator overhead and should
not be read as device figures.

All shipped targets are compile-gated in CI (the standalone consumer is
built for each on every push), with the **full** default surface checked
on the Tier A targets:

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
`kevy-store`, `kevy-hash`, `kevy-bytes`, `kevy-map`, `kevy-madvise`.

CI does not merely compile them for a Cortex-M: it **boots them on
one**. `bench/mcu-probe` is a bare-metal firmware — hand-written vector
table, bump allocator, panic handler, semihosting I/O, because kevy
takes no dependencies and so cannot reach for `cortex-m-rt` or
`embedded-alloc` — that stands up a real `Store` under QEMU
(`mps2-an386`, Cortex-M4) and exercises the KV and TTL surface. It runs
on every push; if the store misbehaves on an MCU, the build fails.

```sh
cd bench/mcu-probe && cargo run --release
```

Measured on that MCU, with a 64 KiB heap:

| | |
|---|---|
| Firmware image | 145 KB |
| Heap for an empty `Store` | **0 B** — the store allocates nothing until you write |
| Heap for 64 sensor keys | 17.8 KB |
| TTL | expires correctly when the firmware advances its own clock |

**What runs on an MCU is `kevy-store`, not `kevy-embedded`.** The
embedded façade (`Config`, background reapers, persistence, the
listener) needs `std`; the MCU gets the storage engine underneath it,
driven directly.

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

- The numbers above are the `core` tier — the floor, not the typical.
  What each capability actually costs, measured on aarch64-musl with the
  `iot` profile (a consumer that genuinely *calls* the tier's API — a
  feature flag you never use is eliminated by LTO and costs nothing):

  | You use | Binary | Delta |
  |---|---|---|
  | KV + TTL (`core`) | 392 KB | — |
  | + durability (snapshot + AOF) | 601 KB | **+209 KB** |
  | + RESP listener | 629 KB | +28 KB |
  | + secondary index | 667 KB | +66 KB |
  | + full-text (BM25) | 691 KB | +24 KB |
  | + vector ANN (HNSW) | 696 KB | +29 KB |

  Durability is by far the largest single line — everything else is a
  small increment on top of it. Empty-store RSS moves the same way:
  336 KB at `core`, 656 KB with every feature compiled in, so the tier
  you pick shows up in resident memory even more than in bytes on disk.
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

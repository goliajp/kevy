# Architecture

A map for someone opening this repository for the first time: where things
are, why the pieces are split the way they are, and where to put a change.

It is deliberately short on detail — each crate's own `lib.rs` header carries
its design, and that is where the detail belongs. This file exists so you know
*which* header to open.

## The one-sentence version

kevy is a Redis-compatible key-value engine written in pure Rust with **no
third-party dependencies**: a thread-per-core, shared-nothing reactor
(io_uring on Linux, kqueue/epoll elsewhere) over a keyspace that also runs
in-process, in a browser, and behind five language bindings.

## Two products, one engine

The single most useful thing to know before reading any code:

```
kevy-embedded  ── the engine, in your process, no sockets, no runtime
      │
      └─ kevy  ── the same engine with a reactor and a wire in front of it
```

Everything else is a door onto one of those two. `kevy-ffi` wraps
`kevy-embedded` in a C ABI; `kevy-jni`, `kevy-napi` and `kevy-wasm` wrap
`kevy-ffi`; `kevy-cli` and `kevy-client` talk to `kevy` over RESP.

If you are adding a capability, it almost always belongs in the engine, and
the doors follow. If it only makes sense with a socket in the picture, it
belongs in `kevy` or `kevy-rt`.

## Layers

The 47 crates form a DAG seven levels deep. **Dependencies only point down**,
and that is a hard rule, not an aspiration — it is what makes the lower crates
publishable on their own.

```
L6  kevy-jni  kevy-napi                       ← language ABIs
L5  kevy  kevy-cli  kevy-client  kevy-ffi  kevy-wasm
L4  kevy-rt  kevy-embedded  kevy-client-async  kevy-cluster-rw  kevy-mcp
L3  kevy-persist  kevy-replicate  kevy-elect  kevy-resp-client
L2  kevy-store  kevy-resp  kevy-window  kevy-sql
L1  kevy-map  kevy-bytes  kevy-seg  kevy-vlog  kevy-index  kevy-scalar
                                                          kevy-lua-host
L0  kevy-alloc  kevy-hash  kevy-ring  kevy-sys  kevy-uring  kevy-time
    kevy-geo  kevy-text  kevy-vector  kevy-ranktree  kevy-compress
    kevy-config  kevy-madvise  kevy-tmpdir  kevy-lua  kevy-scope
    kevy-chaos  kevy-bench  kevy-testnet  kevy-pubsub-bench
```

**L0 and L1 are the stones.** They know nothing about Redis, about a
keyspace, or about each other's callers, and several are published for
their own sake — `kevy-map` is a Swiss-table, `kevy-ranktree` an
order-statistic B-tree, `kevy-seg` an immutable segment file, `kevy-time`
calendar arithmetic. A change here is felt by every caller, so they carry the
strictest rules and the most tests.

**L2–L4 know the domain.** A keyspace, values with expiry, a wire protocol,
a snapshot, a replica. They are where a Redis command's meaning lives.

**L5–L6 are the doors.** Thin by policy: an ABI shell that catches panics,
converts pointers to slices, and delegates. If a door contains logic, that
logic is in the wrong place.

## Where a request goes

One `SET` from a client socket, on the server:

1. **`kevy-sys` / `kevy-uring`** — a readiness event or a completion. This is
   the only place in the workspace that touches libc, by charter.
2. **`kevy-rt`** — the reactor. One OS thread per core, each owning its own
   listener, connections and shard of the keyspace. Threads share nothing;
   they talk over `kevy-ring` SPSC queues when a key belongs to another shard.
3. **`kevy-resp`** — bytes to argv and back. Sans-IO: it never reads a socket,
   which is why it can be tested exhaustively.
4. **`kevy`'s dispatch** — argv to a verb, arity and type checks, the RESP2 vs
   RESP3 reply shape.
5. **`kevy-store`** — the keyspace itself: `kevy-map` for the table,
   `kevy-bytes` for small values, expiry, and the tiering path down to
   `kevy-vlog` / `kevy-seg` when a value gets cold.
6. **`kevy-persist`** — the AOF record and, on its own schedule, snapshots.

Embedded, steps 1–2 are absent: `kevy-embedded` starts at step 4 with argv
you hand it, or at step 5 with a typed call.

## Rules that shaped this

- **Zero third-party dependencies.** Not a boast — a constraint that decides
  designs. It is why `kevy-map`, `kevy-hash`, `kevy-compress`, `kevy-time` and
  the JNI/N-API bindings exist as hand-written crates instead of one line in
  a manifest each.
- **libc only at the OS boundary**, hand-declared with `unsafe extern "C"`
  rather than pulled in as a crate. Five crates declare foreign functions, and
  the list is short enough to state in full: `kevy-sys` (sockets, the
  readiness poller, the self-pipe waker — five blocks), `kevy-uring`
  (`mmap`/`munmap`/`close`/`syscall`), `kevy-alloc` and `kevy-madvise`
  (`mmap`/`munmap`/`madvise`), and `kevy-chaos` (`setrlimit`, in the crash
  harness). Nothing else in the workspace calls out, including `kevy` itself.
  The C ABI that goes the *other* way — `kevy-ffi`'s 23 exports, `kevy-wasm`'s
  31 — is a different thing and lives in the doors.
- **Files ≤ 500 lines, functions ≤ 50.** A pre-commit hook enforces the first.
- **Every `unsafe` block states its premise.** `undocumented_unsafe_blocks` is
  denied workspace-wide.
- **Shared nothing.** No global mutable state, no cross-shard locks. A shard
  that needs another shard's key sends a message.

## Where to put a change

| You want to… | Start here |
|---|---|
| add or fix a Redis command | `crates/kevy/src/dispatch*` + `verb_meta`, then `kevy-store` |
| change how a value is stored | `kevy-store`, and `kevy-bytes` if it is small |
| touch the wire format | `kevy-resp` — and read `bench/resp3gate.sh` first |
| change the reactor or sharding | `kevy-rt` |
| add a language binding | `kevy-ffi` first; the binding wraps that, never the engine |
| change persistence | `kevy-persist`; the tiering half is `kevy-vlog` / `kevy-seg` |
| add an index kind | `kevy-index`, plus `kevy-window` if it can go cold |

Adding a command touches more places than it looks like: the dispatch arm,
`verb_meta`, the RESP3 shape table if the reply changes shape, the coverage
ledger, and the site's derived command reference. `cargo test --workspace
--lib` names every one of them when you miss one.

## Where the proof lives

kevy's claims are checked by machine, not by memory, and the checks live with
the thing they check:

- `bench/` — the arena, the competitor anchors pinned to exact versions, the
  perf ledger with its methodology, and the gates that refuse to produce a
  number when the opponent is not the version claimed.
- `tools/` — the version-alignment gate across seven layers, command coverage
  against the pinned Redis, channel parity that asks each registry rather
  than reading the tree.
- `suite/manifest.toml` — three tiers: `precommit`, `prerelease`, `full`.
  `python3 tools/suite.py precommit` is what runs before every push.

## Reading order

If you are here to learn how it works rather than to change something:

1. `crates/kevy-map/src/lib.rs` — the hashtable, and the clearest example of
   what a stone's header looks like here.
2. `crates/kevy-bytes/src/lib.rs` — the 24-byte value, inline or heap, and
   the union that makes it fit.
3. `crates/kevy-resp/src/lib.rs` — the protocol, with no IO anywhere near it.
4. `crates/kevy-rt/src/lib.rs` — the reactor and why nothing is shared.
5. `crates/kevy-store/src/lib.rs` — where the previous four meet.

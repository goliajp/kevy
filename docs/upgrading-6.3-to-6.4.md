# Upgrading from 6.3 to 6.4

The short version: **no code change, and nothing on disk moves.** No API
was removed, the data directory opens in both directions, and a 6.3.0
node pairs with a 6.4.0 node in either role. Bump the number and you are
done.

```toml
kevy-embedded = "6.4.0"
```

6.4.0 is the quality release, and most of it is invisible from outside:
instruments that were checking less than they claimed, now checking what
they claim. But fixing real defects changes real answers, and seven of
them are observable. Each is below with what you would see before and
after, measured on the published 6.3.0 against 6.4.0 rather than argued
from the diff.

## TL;DR — what to do

| If you… | What changes | § |
|---|---|---|
| run the server or a binding | swap the binary / bump the package; nothing else | — |
| use `GEOSEARCH` / `GEORADIUS` above about 66° latitude | members inside the radius that were dropped are now returned | 1 |
| set `maxmemory` and grow strings with `APPEND` | `used_memory` counts the allocation, so eviction starts closer to the bound | 2 |
| pass `@now±N mo` or `@now±N y` bounds from user input | an offset that cannot fit is now refused, as days always were | 3 |
| use `regexp_*` in the SQL fold face | a capturing alternation followed by more pattern now matches | 4 |
| run on Linux and rely on `CLIENT KILL` or buffer limits | the client's socket now actually closes | 5 |
| scrape `INFO` | `INFO clients` gains `blocked_clients`; `INFO allocator` is new | 6 |
| embed `kevy-seg`, `kevy-ring` or `kevy-time` from Rust | three inputs that corrupted or panicked are refused or saturated | 7 |
| pull `ghcr.io/goliajp/kevy:latest` | pull again; the tag now resolves to 6.4.0 | — |
| use `@goliapkg/kevy-node`, or `@goliapkg/kevy-ts` embedded, from npm | 6.0.0–6.3.0 install but cannot load their engine; 6.4.0 can | [npm](#npm-kevy-node-and-kevy-ts-6063-did-not-load) |

---

## 1. `GEOSEARCH` returns every member inside the radius

The search picked its cell size from the radius alone. A cell's longitude
width in degrees is fixed, but the search box's longitude half-width grows
as `1 / cos(latitude)`, so toward the poles nine cells stopped covering
the box and members inside the radius were silently skipped.

360 members placed at 98% of a 1 km radius around latitude 84 — every one
of them inside it:

| | returned |
|---|---:|
| 6.3.0 | **94** |
| 6.4.0 | **360** |

Nothing was lost up to 66°; by 70° it was 63 of 360, and it grew toward
the pole. If your application compensated — widening the radius, or
re-querying — that is no longer needed. `GEOSEARCH … BYRADIUS 0` also stopped scanning every member of the
key: 9.23 ms on a 200,000-member key before, 0.026 ms now, same answer.

## 2. `maxmemory` counts what a string actually occupies

A string grown by `APPEND` carries its buffer's spare capacity, and that
spare was not charged. 2,000 keys, each built from seven 45-byte `APPEND`s:

| | `used_memory` growth |
|---|---:|
| 6.3.0 | 822,000 |
| 6.4.0 | 912,000 |

The difference is exactly the unused capacity: 45 bytes per key. On eight
appends the two agree, because the buffer happens to be full. So with
`maxmemory` set, a workload that grows values in place now reads a
somewhat higher `used_memory` and starts evicting closer to the bound it
was given, which is what the bound was always meant to mean. Values
written whole with `SET` are unaffected.

## 3. Month and year offsets that cannot fit are refused

`IDX.QUERY` and `IDX.COUNT` accept bounds like `@now-7d`. Days, hours,
minutes, seconds and weeks refused an offset too large to represent;
months and years answered it:

| bound | 6.3.0 | 6.4.0 |
|---|---|---|
| `@now-300000000000y` | `:0` | error |
| `@now-768614336404564650mo` | `:0` | error |
| `@now-1000y` | `:1` | `:1` |

A silent `:0` from an impossible bound is a wrong row set. If you pass
these through from user input, the error is the correct answer; ordinary
offsets behave exactly as before.

## 4. `regexp_*`: a capturing alternation retries its branches

```sql
SELECT regexp_matches('abc', '(a|ab)c');        -- 6.3.0: no rows   6.4.0: {ab}
SELECT regexp_replace('abcd', '(a|ab)c', 'X');  -- 6.3.0: abcd      6.4.0: Xd
```

`(a|ab)c` took branch `a`, left `c` facing a `b`, and never went back for
`ab`. Writing `(ab|a)c` happened to work, which is how it went unnoticed:
the answer depended on branch order. Only capturing parentheses were
affected — `(?:a|ab)c` was always right. This reaches `kevy-cli sql eval`
and anything compiled through `kevy-sql`; the server's own commands do
not use this engine.

## 5. Linux: a disconnect the server decides on reaches the client

On the io_uring reactor, a connection the server chose to close — the
query-buffer or output-buffer limit, or `CLIENT KILL` — could stay open on
the client's side. The server logged and counted the decision; the socket
never sent its FIN. On a runner proven to take the io_uring path, a client
waiting to see the close missed it 5 to 7 times in 100 before, 0 in 100
after.

If a client of yours had a timeout specifically to cope with kills that
never landed, it will now see the connection drop promptly instead. The
defect needed a multishot receive still armed in the kernel, which only
the io_uring reactor uses; the epoll and kqueue (macOS) reactors close
with a plain `close(fd)` and were not affected.

## 6. Two additions to `INFO`

- **`INFO clients` reports `blocked_clients`**, as Redis does: connections
  currently parked in a blocking command, summed across shards.
- **`INFO allocator`** is new, on builds with the `kevy-alloc` feature. It
  splits every mapped byte into named terms — `alloc_live`,
  `alloc_returned` (handed back to the OS), `alloc_virgin` (carved and
  never touched), `alloc_hysteresis` (emptied and held for reuse),
  `alloc_segment_overhead` and a few more — so a resident-memory ratio can
  be explained rather than only measured. On other builds the section is
  absent.

Both are additions. No existing field changed name or meaning.

## 7. For Rust embedders

None of these is reachable from the server's commands; they matter if you
use the crates directly.

- **`kevy-seg`**: `SegBuilder::push` returns an error for a key too large
  to fit a page. Before, a key of 4,074 bytes produced a segment that
  wrote and opened cleanly and could not be read back, and a larger one
  panicked. A single flipped bit in a manifest record's length field can
  also no longer empty the ledger silently, and a page whose header claims
  more slots than fit is refused as corrupt rather than panicking.
- **`kevy-ring`**: `ring(capacity)` with a capacity above the largest
  power of two a `usize` holds saturates there. It used to return a ring
  that failed on its first push.
- **`kevy-time`**: `add_months` saturates instead of panicking;
  `checked_add_months` and `checked_epoch_from_civil` are new.
- **`kevy-bytes`**: `SmallBytes::heap_bytes` now reports the allocation's
  capacity, not its length — the change behind §2.

None of this is a breaking change by Cargo's rules. `cargo-semver-checks`
compared all 40 published library crates at `v6.4.0` against `v6.3.0` and
found nothing that needs a major version (`kevy-mcp` is binary-only and has
no API to compare).

---

## What carries over unchanged

Checked on a Linux box against the published 6.3.0 binary, not assumed:

- **The data directory.** Written by 6.3.0, opened by 6.4.0, reopened by
  6.3.0, with every value intact, including values large enough to live in
  the value log. No migration, no one-way door.
- **Replication.** A 6.3.0 primary with a 6.4.0 replica converges, and so
  does a 6.4.0 primary with a 6.3.0 replica, so you can upgrade one node at
  a time in either order.
- **The wire**, apart from §1, §3 and §6 above.

---

## What else 6.4.0 changed — what a gate can see

Most of the release is about the repository rather than the engine, and
it is written up in full in the [changelog](https://github.com/goliajp/kevy/blob/develop/CHANGELOG.md). In brief:
every `unsafe` block now states the premise it relies on (378 did not);
every public type implements `Debug`; the loom interleaving suites, which
no gate had ever run, now run on every push; the codec's documentation no
longer claims to detect corruption it cannot, and says where integrity is
actually enforced; and several test instruments that were passing without
checking what they named were rebuilt so that breaking the code they guard
makes them fail. None of that changes an answer you receive.

---

## Getting it

```toml
# Cargo.toml
kevy-embedded = "6.4.0"
```

```sh
npm install @goliapkg/kevy@6.4.0          # wasm
npm install @goliapkg/kevy-node@6.4.0     # Node native
pip install kevy==6.4.0
go get github.com/goliajp/kevy-go/v6@v6.4.0
cargo install kevy --version 6.4.0        # the server binary, from source
npm install -g @goliapkg/kevy-bin@6.4.0   # the server binary, prebuilt
```

`@goliapkg/kevy-bin` is new on npm with 6.4.0: `kevy` and `kevy-cli`
without a Rust toolchain, on the same three platforms as the release
binaries and byte-identical to them.

### npm: kevy-node and kevy-ts 6.0–6.3 did not load

`@goliapkg/kevy-node` takes its native engine from a platform package
(`@goliapkg/kevy-node-linux-x64` and so on) pinned at its own version. From
6.0.0 to 6.3.0 those platform packages were never published — npm served
only 5.1.0 — so installing kevy-node succeeded, npm skipped the optional
dependency it could not find, and the first `open()` failed with a
`dlopen` error naming a `target/debug` path. `@goliapkg/kevy-ts` pins the
same packages, so its embedded backend failed the same way; its remote
client (`kevy://host:port`) was unaffected.

6.4.0 is the first version whose platform packages are on npm, published
on 2026-09-16 and checked by installing from the registry on macOS arm64
and Linux x86-64. The earlier versions cannot be repaired after the fact:
upgrade to 6.4.0.

Prebuilt server binaries for Linux (x86-64, arm64) and macOS (arm64), each
with a SHA-256 file, are on the
[v6.4.0 release](https://github.com/goliajp/kevy/releases/tag/v6.4.0).
Container users on `ghcr.io/goliajp/kevy:latest` get 6.4.0 on the next
pull; the pinned tag is `ghcr.io/goliajp/kevy:6.4.0`.

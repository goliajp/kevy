# Perf profile, 2026-06-20, lx64 (v1.22.0 + io_uring nap-fix)

Goal: locate the actual bottlenecks that keep kevy from pulling
further ahead of valkey/redis. Before this profile every lever
discussed was based on code review + estimation; this is the
measurement that turns guesses into facts.

## Setup

- lx64 16-core bare-metal, kernel 6.12.
- kevy built with a new `release-perf` profile: same codegen as
  `release` (LTO + 1 codegen unit + abort) plus
  `debug = "line-tables-only"` and `RUSTFLAGS=-C
  force-frame-pointers=yes` so `perf record --call-graph fp`
  resolves every symbol back to a Rust function.
- `perf record -F 999 --call-graph fp` against the kevy pid for
  the duration of one bench run. Server cpus 0-9, client cpus
  10-15.
- Two workloads:
  - **`-c1` Rust client**: `embed_vs_server --kevy-port 7011
    -N 100000 --no-embed` (single conn, sequential SET + GET).
    Measured 59 k ops/s, p50 ~16 µs.
  - **`-c50 -P16` C client**: `redis-benchmark -c 50 -P 16 -n 3M`.
    Measured 5.98 M SET / 6.00 M GET.

## What I expected vs what perf shows

| layer | I estimated | perf says (-c1) | perf says (-c50 -P16) |
|---|---:|---:|---:|
| syscall / kernel | ~25% | **~32% userspace+kernel mix** | **~12%** |
| RESP decode (`parse_command`) | ~10% | **0.04%** | 2.38% |
| Dispatch (scope/cluster/verb) | ~10% | **0.05%** | 1.95% |
| `Store::set` keyspace work | ~15% | **0.07%** | ~3% |
| Reactor / spin / inbox | ~20% | **~35%** | ~30% |
| Spectre BHB mitigation | not on radar | **10.45%** | 5.39% |

**The estimation is off by an order of magnitude on every line.**
Store / dispatch / RESP combined is < 0.5% at -c1 — the entire
hot-path-optimization conversation up to now was aimed at the
wrong layers.

## Hot symbols — `-c1 Rust client` (the column closest to
valkey/redis)

```
 17.36%  [.] kevy_rt::uring_inbox::uring_drain_inbound
 12.93%  [.] kevy_rt::runtime::Runtime::run::<closure>
 10.45%  [k] clear_bhb_loop                     # Spectre BHB
                                                # mitigation per syscall
  6.03%  [.] syscall (libc)
  4.46%  [k] entry_SYSRETQ_unsafe_stack
  4.41%  [k] fget                                # fd lookup
  3.09%  [k] do_syscall_64
  3.02%  [k] entry_SYSCALL_64
  2.75%  [k] arch_exit_to_user_mode_prepare
  2.58%  [.] kevy_rt::shard_flush::flush_wakes
  2.49%  [.] kevy_rt::shard_flush::flush_backlog
  2.31%  [k] entry_SYSCALL_64_after_hwframe
  2.17%  [k] __do_sys_io_uring_enter
  2.16%  [k] fput
  1.54%  [k] syscall_return_via_sysret
   ...
  0.06%  [.] kevy_store::insert_entry
  0.05%  [.] kevy::dispatch::dispatch_with_proto
  0.04%  [.] kevy_resp::parse_command_borrowed
  0.04%  [.] kevy_rt::exec::start_single
  0.03%  [.] kevy_rt::dispatch_batch
  0.02%  [.] kevy_rt::exec_op::run_dispatch
```

Buckets:

- **Kernel syscall path (kernel + libc syscall wrapper)**: ~32%
- **Spectre BHB mitigation** (`clear_bhb_loop`): 10.45%
- **Reactor spin/inbox/flush overhead**: ~35%
  - `uring_drain_inbound`: 17.36%
  - `Runtime::run::closure` (main loop body): 12.93%
  - `flush_wakes` + `flush_backlog`: 5.07%
- **Actual command work** (RESP, dispatch, Store): ~0.3%

The picture at -c1: the server has **basically nothing to do
per op**. It's spinning the reactor, polling cross-shard inboxes
that nobody is writing to, and paying the kernel for one
`io_uring_enter` per request.

## Hot symbols — `-c50 -P16` C client

```
 13.00%  [.] kevy_rt::runtime::Runtime::run::<closure>
  9.89%  [.] kevy_rt::uring_inbox::uring_drain_inbound
  5.39%  [k] clear_bhb_loop
  2.38%  [.] kevy_resp::parse_command_borrowed
  2.23%  [.] syscall
  2.23%  [k] nft_do_chain                        # iptables eval
  1.95%  [.] kevy::dispatch::dispatch_with_proto
  1.88%  [k] fget
  1.87%  [.] kevy_rt::flush_wakes
  1.72%  [k] entry_SYSRETQ_unsafe_stack
  1.61%  [.] kevy_rt::exec::handle_command
  1.58%  [.] kevy_map::map_keyed::find_by_borrow
   ...
```

Same skeleton, with the work share larger because the server is
saturated:

- Reactor overhead (`Runtime::run` + `drain_inbound` +
  `flush_wakes`): ~25%
- Spectre / kernel: ~13%
- Actual command work (parse + dispatch + handle + map find +
  ...): ~7-8% (real, productive %)

Even at full pipeline saturation `uring_drain_inbound` is the #2
hottest userspace symbol and `Runtime::run::closure` is #1.

## Where the ~13% `Runtime::run::closure` comes from

That's not a workload symbol — it's the main reactor `run_uring`
loop body itself getting attributed because every non-inlined
helper inside it pops up under its closure. Probably a codegen
shape (one giant inlined arm of `match` that the inliner couldn't
flatten into its callers). Worth a deeper look, but the headline
lever isn't this — it's `uring_drain_inbound`.

## The takeaway, restated

1. **Store / dispatch / RESP are not the bottleneck.** At -c1
   they're 0.3%; at -c50 -P16 they're ~7%. Optimizing
   `Reply::write_to` (was A2 on the levers list), zero-alloc
   `parse_command` (was A3), `SmallBytes` inline expansion (A5)
   would be **measuring rounding error**. Drop them.

2. **The bottleneck is the reactor open-loop overhead.** Two
   functions — `uring_drain_inbound` (17% / 10%) and
   `Runtime::run::closure` (13% / 13%) — together account for
   **30%** of CPU at -c1 and **23%** at -c50 -P16. Even when
   the server has zero peer inbound traffic, the cross-shard
   inbox sweep + reactor housekeeping runs every loop iteration.

3. **Kernel syscall path is the second bottleneck**, including
   the Spectre BHB mitigation (10%/5%). That's a kernel/CPU
   cost; the only userspace lever is "do fewer syscalls per
   op" — i.e. SQPOLL mode or batched submissions across
   multiple connections.

## Revised lever ranking (post-profile)

The old A/B/C list went from "predicted-by-estimation" to
"refuted-by-perf". Replace with a measurement-grounded list:

### D1 — conditional `drain_inbound` ⭐⭐⭐

Atomic per-peer "has-inbound" flag set by `send_to`, cleared by
the drain. Reactor checks the flag(s) instead of calling
`pop()` on every empty queue. Single change, no protocol /
behaviour change.

- Predicted gain at -c1: ~15% (drops 17% reactor cost most of
  the way to 0.5% when no cross-shard traffic).
- Predicted gain at -c50 -P16: ~7-9% (cross-shard traffic
  exists for forwarded keys; flag still lets us short-circuit
  empty peers).
- Effort: 1 day.

### D2 — conditional `flush_wakes` / `flush_backlog` ⭐⭐

Same trick on smaller hot functions. Both are 2-3% each;
combined ~5% savings at -c1.

- Effort: half day.

### D3 — inspect `Runtime::run::closure` 13% ⭐⭐

Read the loop body's disassembly with `perf annotate
Runtime::run::closure`. The 13% probably hides one or two
non-inlined helpers that should be `#[inline(always)]` /
restructured. Could be 2% gain, could be 8%; needs the
annotate output to decide.

- Effort: investigation (half day) + implementation (variable).

### D4 — client zero-alloc (was A1) ⭐⭐⭐

Doesn't show in **server** profile, but client-side `Vec<Vec<u8>>`
allocation per `request` call inflates the round-trip total.
Reducing client per-op time = more ops/s on the wire.

- Predicted gain: +15-30% for the Rust-caller -c1 column
  specifically. No impact on C-client `redis-benchmark` numbers
  (server profile unchanged).
- Effort: 1-2 days.

### D5 — `io_uring` SQPOLL feature ⭐

Kernel polls SQ — no `io_uring_enter` syscall per op. This is
the **only** lever that meaningfully cuts the syscall + Spectre
~42% bucket at -c1. Costs 1 CPU core 100% (kernel poller),
opt-in only.

- Predicted gain: 1.5-2× at -c1 (the entire kernel bucket
  collapses if the kernel is already polling).
- Effort: 3-5 days + ops doc.

### D6 — Spectre BHB mitigation off ⭐

Boot kernel with `mitigations=off` or `spectre_v2=off`. Removes
the 10% / 5% `clear_bhb_loop` cost. **Unsafe for hostile-tenant
deployments**, fine for trusted single-tenant boxes.

- Predicted gain at -c1: ~10%.
- Effort: zero (doc only).
- Risk: documented, opt-in via ops, never default.

### Levers dropped (predicted-but-refuted)

- ~~A2 simple-reply fast-path~~ — `encode_bulk` 0.01% at -c1,
  not worth a line.
- ~~A3 server decode zero-alloc~~ — `parse_command_borrowed`
  0.04% / 2.38%; potential gain at high-conc is in noise.
- ~~A4 fixed buffer write~~ — write isn't on the hot list at all
  in the profile; the syscall cost is the `io_uring_enter` itself
  not the buffer copy.
- ~~A5 SmallBytes inline width~~ — Store path is < 0.1%.
- ~~A7 dispatch trie~~ — `dispatch_with_proto` 0.05% / 1.95%; the
  `match` is already fast.

## Recommended next step

Implement **D1 + D2** (conditional drain / flush) as a single
feature branch. Predicted combined gain ~20% at -c1 (so the
kevy-server-Rust column moves from 63 k to ~76 k). Same patch
should give 7-12% at -c50 -P16 (kevy hits ~6.5-7 M / op type).

D3 is investigation-flavoured; do it second, can move the needle
or land at 0 depending on what `perf annotate` shows.

D4 is independent and lives in `kevy-client` — can run in
parallel.

D5 + D6 are configuration-flag work; postpone until D1-D4 has
been measured.

After D1-D4 we re-run this profile; if `uring_drain_inbound` /
`Runtime::run::closure` are still top-3 we got the analysis
wrong and need to dig deeper.

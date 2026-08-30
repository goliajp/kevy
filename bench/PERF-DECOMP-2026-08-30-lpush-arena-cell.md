# LPUSH — Phase A decomposition (arena cell), v6 perf axis

Status: **Phase A complete. No Phase B.** The Pre-Phase-A gate passes — the
gap is real, unlike ZADD's — the cost model closes to 0.8%, the first
candidate is priced and refuted before a line was written, and the
Pre-Phase-B gate then refuses the round: the only userspace thing above the
bar is the PAUSE instruction of seven shards waiting for the one that owns
the key, which is the topology of a one-key benchmark rather than work.
Nothing implemented, deliberately.

Companion to `PERF-DECOMP-2026-08-30-zadd-arena-cell.md`. The ROADMAP names
two narrow cells; that one covers ZADD.

---

## Pre-Phase-A gate: this one passes

ZADD's stated gap did not survive its gate — the ledger's own rule read it
as NOISE. LPUSH's does.

Exclusive box, nine interleaved rounds, a fresh server each round so the
list starts empty on both sides:

| | median | stdev |
|---|---:|---:|
| kevy | 3,140,172 | ±71,302 (2.3%) |
| Redis 8 | 2,907,540 | ±83,216 (2.9%) |

232,632 apart against a tolerance of 83,216 — **2.8x the band. 1.08x, and
it is a real 1.08x.**

It is a narrow *lead*, not a loss. The work is widening it.

---

## S01 — the cell grows without bound

`redis-benchmark -t lpush` sends `LPUSH mylist <one 20-byte value>` and
never trims. At three million operations a second, a three-second window
appends about **nine million elements to one list**, so the measurement is
of an append to a structure that is two orders of magnitude larger at the
end of the window than at the start, and the server allocates a few hundred
megabytes doing it.

That is the opposite shape from the ZADD cell, which writes the same member
to a one-element sorted set forever. The two cells share a name in the
ledger and nothing else, which is why they get separate passes.

---

## S02 — the cost model: per command and per element

`LPUSH` takes any number of values, and the arena sends one. Sending more
separates what a command costs from what an element costs — the same
server, the same protocol, `median-of-5`, one key per arity:

| values per command | ns/cmd | ns/element |
|---:|---:|---:|
| 1 | 335.4 | 335.4 |
| 2 | 439.0 | 219.5 |
| 4 | 640.5 | 160.1 |
| 8 | 1035.3 | 129.4 |

Fitting the two extremes: `(1035.3 - 335.4) / 7` = **100.0 ns per element**,
and the intercept is **235.4 ns per command**.

The middle two rows are the check, and they are not part of the fit:

| N | model | measured | |
|---:|---:|---:|---|
| 2 | 435.4 | 439.0 | 0.8% |
| 4 | 635.4 | 640.5 | 0.8% |

**So the arena's cell — one value — spends 70% of its time on the command
and 30% on the element.**

---

## S03 — what a keyspace lookup costs, and the candidate it kills

The ZADD decomposition recorded a candidate found by reading:
`list_push_one` calls `list_value_for_push` twice, once to ask `is_none()`
and once to take the value; `account_delta` looks the key up a third time
and `lpush` a fourth through `list_len`. Four lookups where one would do.

Priced before attacked. `PING` does not touch the keyspace, `LLEN` touches
it once, and both are RESP arrays — the inline `PING` the first attempt used
is a different parse path, and it is 2.6 ns cheaper, which is small but is
not zero:

| | ns/op |
|---|---:|
| PING (RESP array) | 133.5 |
| PING (inline) | 136.0 |
| LLEN on a 50,000-element list | 141.6 |
| LPUSH, one value | 333.4 |

**A keyspace lookup costs 8.1 ns.**

So the four lookups are 32.4 ns — **9.7% of the operation**, and removing
the three redundant ones would recover about 24 ns, or **7%**. Real, but not
what the reading suggested, and the restructuring it needs is not small: the
double lookup is what the borrow checker leaves when a mutable borrow cannot
span the question and the answer.

The note in the ZADD document is corrected accordingly. It was written from
a source reading with no price attached, which is exactly the shape this
project keeps finding — a candidate that sounds decisive until it is
measured.

---

## S04 — where the time actually is

Subtracting what is now priced, from the one-value cell:

| | ns | |
|---|---:|---|
| protocol: parse, dispatch, reply | 133.5 | measured, `PING` |
| per-element: 2 lookups | 16.2 | measured, 2 x 8.1 |
| per-element: the append itself | **83.8** | the remainder of 100.0 |
| per-command: 2 lookups | 16.2 | measured, 2 x 8.1 |
| per-command: everything else | **85.7** | the remainder of 235.4 - 133.5 |
| **total** | **335.4** | measured 335.4 |

The two bold rows are the decomposition's output: **~84 ns to append one
20-byte element**, and **~86 ns of per-command work that is neither the
protocol nor a lookup** — the write path's `Arc::make_mut`, the accounting,
the propagation bookkeeping, the integer reply.

Against Redis, using the gate run's numbers (a different session from the
model above, and about 5% faster on this box — the two are not mixed):

| | total ns/op | minus its own GET |
|---|---:|---:|
| kevy | 318.5 | 181.9 |
| Redis 8 | 343.9 | 167.9 |

kevy's lead is entirely its cheaper base path — 39.4 ns/op ahead before any
list work — while its list-specific work is **14.0 ns dearer**. Widening the
1.08x means attacking one of the two bold rows, not the protocol.

---

## S05 — the Pre-Phase-B gate, and what it says

The gate is that a target shows double-digit percent of self-time before it
is attacked. Taken on the `profiling` build — release codegen with symbols,
because `[profile.release]` strips and a perf record of it resolves nothing
but libc and the kernel — at 3,139,718 ops/s, which is the release binary's
own range, on a list that reached 35 million elements during the window.

By symbol, one thing clears 10pp and it is the LTO aggregate:

| | self |
|---|---:|
| `run_uring` (everything inlined into the busy-poll body) | 19.7% |
| `syscall` | 2.7% |
| `KevyMap` lookup | 2.1% |
| `dispatch_batch` | 1.4% |
| `malloc` / `cfree` | 1.3% / 0.7% |

`debug = 1` on that profile makes the aggregate splittable by source line,
which the earlier decompositions could not do:

**19.68% of all samples is `sse2.rs:25`.**

Which is not what it looks like. `sse2.rs` is `core::core_arch::x86`, and
the first guess — a vectorised memory copy, on a workload that appends
20-byte values — was wrong. Expanding the inlined frames
(`perf report --inline`) attributes it to **`uring_reactor.rs:442`**:

```rust
if self.xshard_inflight > 0 {
    std::hint::spin_loop();
    continue;
}
```

`std::hint::spin_loop()` on x86 is `_mm_pause()`, and `_mm_pause` lives in
`sse2.rs`. **The 19.68% is the PAUSE instruction.**

And by call graph, with dwarf unwinding:

```
run_uring  62.72% inclusive
 └─ IoUring::submit_and_wait (inlined)  43.71%
     └─ syscall  43.57%
         └─ do_syscall_64  35.28%
             └─ syscall_exit_to_user_mode  16.97%
                 ├─ __audit_syscall_exit  5.81%
                 └─ audit_reset_context   5.76%
```

Three readings fall out, and the gate's verdict follows from them.

**A fifth of the machine is seven shards waiting for one.** The arena's
LPUSH cell writes to a single key, so one shard owns `mylist` and the other
seven forward to it and spin on that line until the reply comes back. The
spin is deliberate and the comment above it says why — staying in the spin
rung beats a kernel sleep and wake per reply batch — and those seven cores
have nothing else to do, so the cycles are idle rather than stolen. **It is
not an attack target. It is the topology of a one-key benchmark, visible.**

**The syscall path is 44% of the reactor**, and inside it this box's audit
subsystem is over 13% of all samples. That is a property of the measuring
machine, not of kevy — it taxes every engine in the arena identically, so no
ratio in the ledger is distorted by it, but absolute ops/s from lx64 carry
it and a profile of any command on this host will show it.

**Nothing in userspace that is actually work clears the bar.** After the
PAUSE and the syscall path, the largest is a `KevyMap` lookup at 2.1%, then
`dispatch_batch` at 1.4%, then `malloc` and `cfree` at 1.3% and 0.7%. The
84 ns per element and the 86 ns per command are spread across inlined code
in single-digit fractions, not concentrated anywhere.

**So the gate says no, and this pass ends without a Phase B.** That is the
gate working: the methodology's own record has a round that attacked
userspace memcpy in this reactor and measured throughput-neutral, and
another that attacked the spin limit and measured the same. Attacking a
fifth of the samples that turns out to be idling would have been the third.

## Open

1. **What the 84 and 86 ns rows are made of** is still unnamed. The profile
   says they are spread through inlined code in single-digit fractions
   rather than sitting in a symbol, and the one line that stands out is not
   work. Naming them would need instruction-level work inside the reactor,
   or a differently-shaped measurement — a multi-key LPUSH cell would put
   all eight shards to work and change what the profile is a profile of.
2. **The unbounded growth is part of the measurement.** Whatever the profile
   says is an average over a list that spans a hundredfold during the
   window; a reading at a fixed length would say something different and is
   probably also worth taking.
3. **The four lookups**, worth ~7%, remain a candidate rather than a
   dismissal — but they are behind whatever the profile finds in the 84 and
   86 ns rows.

# LPUSH — Phase A decomposition (arena cell), v6 perf axis

Status: **Phase A.** The gate passes, the cost model closes to 0.8%, and the
first candidate is priced and refuted before a line was written. Nothing
implemented. The profile that would name what remains is not taken.

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

## Open

1. **The profile.** Neither bold row is named yet, and the Pre-Phase-B gate
   says a target must show double-digit percent of self-time before it is
   attacked. The `profiling` profile exists for this — release codegen with
   symbols, because `[profile.release]` sets `strip = true` and a perf
   record of it resolves nothing but libc and the kernel.
2. **The unbounded growth is part of the measurement.** Whatever the profile
   says is an average over a list that spans a hundredfold during the
   window; a reading at a fixed length would say something different and is
   probably also worth taking.
3. **The four lookups**, worth ~7%, remain a candidate rather than a
   dismissal — but they are behind whatever the profile finds in the 84 and
   86 ns rows.

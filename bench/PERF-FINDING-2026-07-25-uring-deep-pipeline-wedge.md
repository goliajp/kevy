# Reactor flow-control wedge under a deep pipeline of large values (pre-existing, orthogonal to tiering)

**Status:** ROOT-CAUSED and reproduced; **NOT a capacity-arc regression** —
it reproduces on a plain, non-tiered server. Recorded for a future reactor
fix; it does not block the capacity arc. The capacity-envelope B6 load was
changed to bound in-flight bytes so it measures the capacity gate (demotion
bounding RSS) instead of tripping over this.

## Symptom

The B6 capacity phase (5M × 4KiB SET on a 2GB budget) hung: the driver
timed out on a reply, and every shard reactor sat in `io_cqring_wait`
(0% CPU) — a blocked deadlock, not a busy loop.

## What it is NOT

- Not tiering: a **plain server with tiering off** wedges the same way.
- Not scale / memory pressure: it wedges after ~4096 keys (~16 MB) on a
  box with 47 GB free; demotion has not even engaged (`demotions_total=0`,
  `used_memory` well under budget).
- Not the RowValues rescan (that was the D1 stall, fixed separately): B6
  has no index, so no `VALUES` side-channel exists.

## Isolation (plain server, 4KiB SET values)

| pipeline depth | bytes per send | result |
|---|---|---|
| 512 | ~2 MB | **WEDGE** at ~4096 keys (~8 batches), `io_cqring_wait`, 0% CPU |
| 32  | ~128 KB | completes 1.5M keys in 17.5 s (~350 MB/s), clean |

The trigger is **in-flight bytes per pipelined send**, not key count, value
size, tiering, or total data. A ~2 MB blind pipelined send wedges; ~128 KB
does not. The shape is a classic flow-control deadlock: the server applies
recv backpressure (per-conn output cap) while the client is mid-`sendall`
of a batch larger than the socket buffers, so the client blocks in `send`
before it ever reads the replies that would let the server drain — neither
side progresses.

## Consequence for the envelope

B6 exists to prove the capacity property (demotion keeps RSS bounded while
ingesting ≥10× data:RAM), which is unrelated to this reactor bug. The
`load-b6` driver now bounds each pipelined send to ~128 KB (a realistic
bulk-load client). With that, the real capacity behaviour is finally
measurable and passes: 5M × 4KiB (20 GB) ingests on a 2 GB budget in ~67 s,
`demotions_total=3,534,600`, vlog 14.5 GB on disk — demotion engaged and
spilled as designed.

## Follow-up (out of scope for the capacity arc)

The reactor should not wedge on a large pipelined send — it should apply
backpressure without deadlocking (e.g. keep draining recv, or bound the
accepted-but-unprocessed input so it never blocks the client mid-send).
This is a pre-existing reactor-layer issue (kevy-rt uring/epoll conn flow
control), separate from the tiering/index work, and wants its own change +
a regression test (a deep-pipeline bulk-load smoke). Left for a dedicated
reactor pass — it is not introduced by, and does not gate, the capacity arc.

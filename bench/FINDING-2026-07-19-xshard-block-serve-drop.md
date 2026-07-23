# A cross-shard blocking serve can lose the element it popped

**Status: PARTIALLY FIXED. Escrow (2026-07-21) closed window 1. The
delivery peek (2026-07-23) closes window 2 on kqueue (macOS) and uring has
no such window, but the epoll fallback (`KEVY_IO_URING=0`) keeps a residual
~10% loss (18/20 on lx64) — the peek is a point-in-time check and cannot be
perfectly synchronised with delivery. The complete fix ties escrow release
to the write result and is a larger reactor change; see "Residual on epoll"
below. Design: `.claude/rfcs/2026-07-21-xshard-block-serve-escrow.md`.**

## Residual on epoll — the peek's ceiling (2026-07-23)

The peek asks the kernel at the delivery *decision*, but the decision and
the actual send are not one atomic step, so a point-in-time "alive" can go
stale before `ack_serve` releases the escrow. On kqueue the timing never
lost the race across every run tried; on epoll it loses ~10% (measured
18/20, `*0` — element lost). By elimination this is the peek-returns-alive
path: `*0` requires `abandoned` false AND the conn present AND
`peer_gone()` false, which is exactly a stale "alive". uring never had
window 2 (it sets `abandoned` at EOF detection), and Linux defaults to
uring, so the residual is specific to the explicit epoll fallback.

Adding an `eprintln` probe flipped the epoll failure to a pass — a
Heisenbug confirming it is timing, not a logic branch, and the reason the
seam that determinises kqueue does not fully determinise epoll.

The sound completion is the write-result approach I set aside for the peek:
do not release the escrow at deliver time; buffer the reply, and release
only once the conn's output flushes without error, restoring on write
failure or teardown. That removes the point-in-time race entirely, but it
touches the reactor write path in both reactors — code whose blast radius
is *every* reply, not just block serves — so it is a steel-layer change
that wants an explicit decision before it lands, not an autorun patch.

Also fixed this round, independent of the windows: a non-empty serve reply
arriving after the origin record is already gone stranded the escrow on the
target (element lost). `origin_on_serve_resp` now routes that restore by the
key's owning shard.

## Window 2 (found 2026-07-23) — the mechanism

The peek closes this on kqueue/uring; see the epoll residual above.

The escrow fix closes the window where the origin's block record is *gone*
by reply time: the target holds the element until the origin says
delivered-or-abort, and `abandoned` marks a disconnect noticed while
`serving`. A second window remained, and it lost an element on the macOS
CI runner (`a_disconnect_during_the_serve` FAILED, LRANGE showed `*0` — an
empty list, the element gone; local single runs pass 10/10, it only
surfaced under the full parallel suite's load).

Both the serve reply (`BlockServeResp`) and the disconnect detection
(`close_conn` → `cancel_xshard_on_close`) are events on the *origin*
shard's single-threaded loop, both after `serving = true`. Their order is
not guaranteed:

- disconnect first → `abandoned = true` → reply runs `abort_serve` →
  escrow applied, element restored. Correct.
- **reply first → `abandoned` is still false → `deliver_block` buffers the
  reply into `c.output` of a conn that is about to be reaped, and
  `ack_serve` releases the escrow. Then the disconnect is processed and
  the conn is dropped. The element was popped, "delivered" to a dead
  output buffer, and the undo released — lost.**

`deliver_block` (block_xshard.rs) writes to `c.output` unconditionally; it
has no way to know the client is gone, because the FIN may not have been
read yet. On a loaded runner the origin shard can process the reply
(arriving at the serve-delay deadline) before it polls the FIN from the
dropped socket — so `abandoned` is false at reply time even though the
client left long before.

## The fix: ask the kernel at delivery

`abandoned` is this shard's *processed* view of the disconnect, and it can
lag the truth by the reactor's completion backlog. So instead of trusting
it alone, `origin_on_serve_resp` asks the kernel directly right before
delivering: `Socket::peer_gone()` does a non-blocking
`recv(MSG_PEEK|MSG_DONTWAIT)`, which reports the peer's FIN whether or not
this shard has reaped the recv completion that carries it. A dead socket —
or a conn already reaped — routes to `abort_serve` (restore) rather than a
buffered-and-released delivery. The peek's one blind spot (unread data
ahead of the FIN hides it) does not apply here: a blocked BLPOP client sent
its command and nothing more, so its buffer is empty and the FIN is what
`recv` returns.

Chosen over tying escrow release to the async write result — which would
have been correct too but reworked the reply path across both reactors —
because the peek is localized to the one delivery decision and is correct
by construction: it reads the authoritative kernel state at the exact
moment the decision is made.

## Verified deterministically, not by load luck

An earlier header read "Status: FIXED" after the escrow round, then was
corrected to "PARTIALLY FIXED" when the second window surfaced — a false
"fixed" on a data-loss defect is the worst kind, so it was walked back
until there was proof. That proof now exists. A debug-only seam
`KEVY_TEST_XSHARD_HOLD_CLOSE` defers the serving conn's teardown to
reproduce the exact reply-before-disconnect ordering (the existing
`KEVY_TEST_XSHARD_SERVE_DELAY_MS` only controls the serve delay). With the
peek removed the regression fails `*0` (element lost); with it, `*1`. Plus
a `peer_gone` unit test on live / has-data / dropped sockets. The
io_uring path never had the second window — it sets `abandoned` at EOF
detection — so this is a poller-path fix, which is why the failure was
macOS-only.

## What the 2026-07-21 escrow round did establish (still true)

The fix is escrow: the target captures the undo by reading the element
*before* it pops, and holds it until the origin confirms delivery. Two
things this finding originally assumed turned out to be wrong, both found
by reading the code — the target does not know the element it popped (it
replays a command and gets RESP bytes, same as the origin), and two of the
six block kinds consume nothing at all, so "restore the element" is
undefined for them.

The seam `KEVY_TEST_XSHARD_SERVE_DELAY_MS` (debug builds only) makes the
*first* window deterministic. It does not cover the reply-before-disconnect
ordering above, which is why that window passed review.

Original report follows.

---

**Status when filed: open. Real, pre-existing, not introduced by that session's work.**
Filed with the code path and the evidence rather than fixed, because the
correct fix is a protocol decision, not a patch.

## The defect

`crates/kevy-rt/src/block_xshard.rs:291-294`:

```rust
pub(crate) fn origin_on_serve_resp(&mut self, conn: u64, _key: Vec<u8>, reply: Vec<u8>) {
    let Some(ob) = self.origin_blocks.get_mut(&conn) else {
        return; // conn timed out / disconnected during the serve
    };
```

`reply` carries the element the target **has already popped**. When the
origin no longer has a block record for `conn`, the function returns and
the reply is dropped. The element is gone from the list and was delivered
to nobody. The comment names the case; the handling discards the payload.

## How it is reached

The serve is origin-initiated, and the origin does check liveness before
starting one — `origin_on_ready` (`:217-220`) returns early when
`origin_blocks` has no entry, and `cancel_xshard_on_close` removes that
entry on disconnect. So the window is narrow but real:

1. Origin sends `BlockServeReq` to the target.
2. The client disconnects; `close_conn` → `cancel_xshard_on_close` removes
   the origin record.
3. The target pops the element and replies `BlockServeResp`.
4. `origin_on_serve_resp` finds no record → **element lost**.

A push racing an in-flight cancel reaches the same place from the other
side: the target still has the waiter armed, serves it, and the origin has
already torn down.

Redis does not have this race — it serves blocked clients synchronously
inside the same event loop. It exists here because the serve is
asynchronous across shards, which is the price of the thread-per-core
model, not an oversight.

## Evidence

CI, `test (x86_64-unknown-linux-gnu)`, run 29699734986:
`blpop_remote_disconnect_then_push_is_clean` got `*-1\r\n` where the
element was expected — the list was empty at the fresh `BLPOP`, so the
push had been consumed and discarded. The same test passes 18/18 locally,
including under 12-way CPU contention: the window only opens when cancel
propagation is slow enough to lose the race with the push.

`blocking_cross_shard.rs` has a recorded history of failing on GH Actions
x86_64 while passing on real hardware. That history is probably this.

## Why it is not fixed here

Restoring the element is not a one-liner, and each candidate is a design
choice:

- **Re-push from the origin.** The reply is RESP, and its shape differs
  per block kind (`BLPOP`/`BRPOP` return a 2-array, `BZPOPMIN` a 3-array,
  `BRPOPLPUSH` has already moved the element to a destination and is
  handled by `serve_via_list_move`). Parsing the element back out of a
  reply, per kind, to re-push it is fragile in exactly the way that
  produces a second bug.
- **Restore message to the target.** Cleaner, but it is a new cross-shard
  message with per-kind restore semantics — where does the element go
  back, head or tail, and what does that mean for the ordering another
  waiter has already observed?
- **Two-phase serve** (reserve, deliver, commit). Correct by construction
  and the most expensive: it puts a round trip on the blocking-pop path
  that today costs one.

Whichever is chosen changes the cross-shard block protocol, so it belongs
in a design round, not in a session that happened to trip over it.

## What was done instead

`blpop_remote_disconnect_then_push_is_clean` waited 40ms for the cancel
broadcast before pushing. That wait is now 500ms, so the test asserts what
it is actually for — that cancel-on-disconnect *happens* — instead of
flaking on whether it happens fast enough. The test is deliberately **not**
made to assert the data-loss race: widening the wait makes that race less
likely to be hit, and a test that only sometimes exercises the defect it
is meant to guard is worse than one that clearly guards something else.

A test that targets this race directly needs to force the ordering
(disconnect between `BlockServeReq` and `BlockServeResp`), which needs a
seam the code does not currently expose.

# A cross-shard blocking serve can lose the element it popped

**Status: open. Real, pre-existing, not introduced by this session's work.**
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

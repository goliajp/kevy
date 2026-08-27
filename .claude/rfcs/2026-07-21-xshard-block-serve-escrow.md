# RFC — closing the cross-shard block-serve element drop

Design round for `bench/FINDING-2026-07-19-xshard-block-serve-drop.md`,
which filed the defect rather than fixing it because the fix is a
protocol decision. This is that decision.

**Status: SUPERSEDED — the defect is FIXED (2026-07-23). This RFC's first
escrow sketch was refuted by the code (see the correction at the end), and
escrow itself closed only one of two windows. The complete fix ties escrow
release to the write result and is written up, with its three-reactor
deterministic verification, in
`bench/FINDING-2026-07-19-xshard-block-serve-drop.md` — read that for the
shipped solution. This document is kept for the design history.**

## The defect in one line

The target pops an element and sends it to the origin; if the origin's
block record is gone by then, the reply is dropped and the element is
lost — popped from the list, delivered to nobody.

## What reading the code changed

The finding treated this as an open-ended protocol problem. It is
narrower than that, because **the protection already exists and one path
does not use it.**

`OriginBlock::serving` (`block_xshard.rs:52`) is documented as:

> Suppresses a second concurrent serve AND the timeout sweep, so a serve
> that pops data is never discarded by a timeout firing in the same
> window.

That is precisely the invariant being violated — but only the *timeout*
path honours it. `cancel_xshard_on_close` (`:380`) removes the record
unconditionally:

```rust
pub(crate) fn cancel_xshard_on_close(&mut self, conn: u64) {
    if let Some(ob) = self.origin_blocks.remove(&conn) {
```

So the defect is not a missing design. It is an existing design with one
unguarded entry point. That narrows the fix and rules out the more
speculative options the finding listed.

## Why "just keep the record" is not enough

Honouring `serving` on disconnect makes `origin_on_serve_resp` find its
record, which stops the silent drop. It does not answer what to do with
the element: the conn is gone and cannot receive it.

The origin cannot put it back. It holds `reply` — a RESP frame whose
shape differs per block kind (`BLPOP` a 2-array, `BZPOPMIN` a 3-array).
Parsing the element back out per kind to re-push it is the fragile path
the finding already rejected, and it is fragile in the way that produces
a second bug.

## Decision — escrow on the target

**The element never travels without an owner. The target keeps it until
the origin confirms delivery.**

- `target_serve` pops as it does today, builds the reply, and **retains
  the popped element in an escrow** keyed by `(origin, conn)`.
- The origin delivers to the client and sends `BlockServeAck { conn }`;
  the target drops the escrow entry.
- The origin cannot deliver — the conn disconnected mid-serve — and
  sends `BlockServeAbort { conn }`; the target **restores from its own
  escrow** and re-runs its serve loop for that key.

The property that makes this work: **the raw element is never parsed out
of a reply.** The only component that reconstructs it is the one that
took it apart, holding the shard lock for that list, which already knows
the kind and the end it came from. Per-kind restore is three lines where
the pop is, not a parser.

### Cost

One extra message on the happy path. It is fire-and-forget and off the
latency path — the client has already been written to and unparked
before the ack is sent — so the blocking-pop round trip is unchanged.

Escrow memory is one element per in-flight serve, bounded by the number
of cross-shard-blocked conns. An entry cannot leak: every serve resolves
in an ack or an abort, and both are sent from the origin's own shard,
which does not fail independently of the one holding the escrow.

### Rejected alternatives

- **Re-push from the origin** — needs per-kind reply parsing. Rejected in
  the finding, and reading the code did not improve it.
- **Two-phase reserve/commit** — the reservation has to make the element
  invisible to a concurrent local `LPOP`, which means removing it from
  the list, which is escrow with more steps and a worse name.

## The ordering anomaly, stated rather than hidden

Between the pop and the abort, another waiter on the same key can be
served the *next* element. The escrowed element is then restored to the
head and served afterwards — so a later element reached a client before
an earlier one.

This is real, it is narrow (it needs a disconnect during an in-flight
serve *and* a second waiter on that key), and it is the honest price of
serving blocked clients across shards. Redis does not have it because it
serves synchronously in one event loop; the finding says as much, and
that is a property of the thread-per-core model rather than an oversight.

**Losing the element is not acceptable; reordering under this race is.**
That trade is the decision, and it goes in `docs/` rather than staying in
a commit message, because a consumer building on blocking pops should be
able to find it.

## Implementation sketch

1. `OriginBlock` gains `abandoned: bool`. `cancel_xshard_on_close` sets
   it and keeps the record when `serving`, removes it otherwise (matching
   what the timeout sweep already does).
2. `origin_on_serve_resp`: on a non-empty reply for an abandoned record,
   send `BlockServeAbort` and drop the record; otherwise deliver, then
   send `BlockServeAck`.
3. Two new `Inbound` variants; `inbox.rs` dispatch alongside the existing
   `BlockServeReq` / `BlockServeResp` / `BlockCancel`.
4. Target: `escrow: HashMap<(usize, u64), EscrowedElement>` on
   `XShardWaiters`, written in `target_serve`, consumed by ack/abort.
   `EscrowedElement` carries the kind and the end it was taken from.
5. `target_cancel` must not strand an escrow entry for the same conn.

## Testing

The finding notes the honest problem: this race cannot be hit reliably
without forcing the ordering, and widening a timing window to make it
*less* likely is how the current test ended up guarding something else.

So the test needs the seam: a way to interpose between `BlockServeReq`
and `BlockServeResp` and close the conn there. That seam is worth adding
for this — a test-only hook on the target's serve path — because the
alternative is a test that sometimes exercises the defect, which the
finding already argues is worse than one that clearly does not.

Directly assertable without any race: escrow is empty after a normal
serve, an abort restores the element and the list length is unchanged,
and a restored element is served to the next waiter.


---

# CORRECTION — the escrow design does not work as written

Written after reading `target_serve` and `cmd_block_serve.rs` properly,
before writing any code. The central claim above is false.

## What I got wrong

The design rests on this sentence:

> The only component that reconstructs it is the one that took it apart,
> holding the shard lock for that list, which already knows the kind and
> the end it came from.

**The target does not take the element apart.** `target_serve`
(`block_xshard.rs:457`) replays the frozen `serve_argv` through the
normal dispatcher:

```rust
RespVersion::V2 => self.commands.dispatch_into(&mut self.store, &argv, &mut reply),
```

`block_serve_argv` maps each kind to an ordinary command — `BLPOP key 0`,
`BZPOPMIN key 0`, `BRPOPLPUSH src dst 0`. So the target ends up holding
exactly what the origin holds: **RESP bytes**. There is no popped element
sitting anywhere to put in escrow. "Escrow on the target" is the same
reply-parsing problem, moved one shard sideways and given a better name.

## What else that changes

- **`XReadBlock` / `XReadGroupBlock` do not pop.** `XREAD` is
  non-destructive and `XREADGROUP` creates PEL entries. "Restore the
  element" is not even well-defined for two of the six kinds, and the
  original finding did not notice this either.
- **Key-level snapshot/restore is wrong here.** Reusing `clone_with_ttl`
  from the `atomic()` rollback is tempting and available, but that
  rollback is safe only because the shard lock is held for the whole
  closure. Here the abort arrives asynchronously, so restoring the key
  would silently discard any push that landed in between.

## The revised option space

With a command-replay serve, recovery needs per-kind knowledge. There is
no type-agnostic option; that was the illusion escrow created.

1. **Per-kind restore beside the per-kind serve.** `cmd_block_serve.rs`
   already has one function per kind building the serve argv. Add its
   twin: given kind and reply, produce the restore argv (`LPUSH key elem`
   for `BLPOP`, `RPUSH` for `BRPOP`, `ZADD key score member` for
   `BZPOPMIN`, nothing for the two stream kinds). This *is* the reply
   parsing the finding rejected — but the objection was to doing it in
   the arbiter, far from the shapes. Beside the function that defines
   the reply shape, tested in the same file, it is ordinary code.
2. **Make the pop conditional.** Requires a liveness confirmation that
   can itself go stale between check and pop; shrinking the window
   without closing it.
3. **Accept and document the loss.** Not acceptable for a store people
   are putting financial data in.

Option 1 is the live candidate, and the ordering-anomaly trade in the
section above still applies to it unchanged.

## Why this correction is here rather than silently fixed

The escrow design read well and was wrong, and the thing that caught it
was reading `target_serve` instead of trusting a sketch of it. The same
mistake with the same shape appeared earlier in this work: a durability
fix that was written up as complete before it was measured. Leaving the
refuted version visible is cheaper than a plan that looks endorsed.

---

# REVISION 2 — escrow works after all, if the element is captured by peek

The correction above is right that the target learns nothing from
popping. It does not follow that escrow is dead: the target can capture
the element **before** it pops, by reading it.

## The move

Before dispatching `serve_argv`, the target asks the command layer for a
restore command for this kind and key:

- `Blpop` → peek head (`LINDEX key 0`) → restore is `LPUSH key <elem>`
- `Brpop` → peek tail (`LINDEX key -1`) → restore is `RPUSH key <elem>`
- `Bzpopmin` → peek min → restore is `ZADD key <score> <member>`
- `XReadBlock` / `XReadGroupBlock` → `None`, nothing is consumed

That argv goes in escrow keyed by `(origin, conn)`. On ack it is
dropped; on abort it is dispatched — an ordinary command through the
ordinary path, on the shard that owns the key.

## Why this beats parsing the reply

- **No parsing at all.** The restore is built from typed reads, not from
  bytes that were formatted for a client.
- **Protocol-independent.** `target_serve` dispatches RESP2 or RESP3
  depending on the waiter's `proto`, and reply shapes differ between
  them. A parser would have to be right about both, forever. A peek is
  the same in either.
- **The peek is exact.** It runs on the target, on the same thread,
  immediately before the pop, with nothing interleaved — so what it read
  is what the pop takes.

Cost is one extra read on the cross-shard serve path. `LINDEX` at either
end is O(1); the peek is not on the client's critical path in any case,
since the reply is what the client waits for.

`BRPOPLPUSH` needs none of this: cross-shard it goes through
`serve_via_list_move` and the list-move orchestrator, not through
`origin_on_serve_resp`, so it is outside this defect.

## Status

**Designed, not implemented.** Verification of the current HEAD comes
first; this is a protocol change and it should land on a branch with a
clean baseline behind it, not be half-built in a tree that is being
verified.

Three revisions is the honest cost of a design done by reading code
rather than by sketching: v1 assumed the popper knew the element, the
correction showed it did not, and this revision found the seam that was
actually available. Each step came from opening the file rather than
reasoning about what was probably in it.

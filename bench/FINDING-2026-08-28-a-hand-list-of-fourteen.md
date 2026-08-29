# FINDING 2026-08-28 — a hand-written list of fourteen, and the four things it never found

**Status**: all four fixed, each red-green. The sweep that found them is a
permanent cell: `every_documented_verb_refuses_a_short_call_in_redis_words`
in `crates/kevy/tests/differential_wire_vs_embedded.rs`.

## Why the list could not have found them

The wire differential harness carried `ARITY_PROBE` — fourteen verbs that
`cmd_resolve.rs` guards by arity, called with the wrong count. It was
written for a real defect and found it: **twelve of those fourteen reported
a command that exists as one that does not**.

It also could not have found anything else, because a hand list contains
what someone thought to write down. The fourteen were the arity-guarded
extension verbs, because that was the shape under suspicion at the time.

What changed is not the harness. `kevy_resp::verb_arity` moved the arity
column out of `kevy` — which is cement, and which `kevy-embedded` may not
depend on — into a crate both surfaces read. That was done to stop the
facade restating `argv.len() < 4` beside a comment naming its source. The
side effect is that a TEST outside `kevy` can now enumerate every
documented verb and its declared minimum, so the list became a sweep:

```
probed 165, skipped 26 (arity ±1 — a call cannot carry fewer parts than
the verb), unknown 0
```

Only the too-FEW direction is swept, deliberately. A call below the
minimum cannot reach a handler, so the sweep can never execute anything —
no SHUTDOWN, no FLUSHALL, no exemption list to maintain.

## 1. `XPENDING k` panicked the shard thread

```
crates/kevy/src/dispatch_stream/group.rs:391
argv-borrowed index out of bounds
```

`parse_xpending_extended` opens with `let mut i = 3; if args[i]...`.
`cmd_xpending` sends anything that is not exactly three arguments there,
and nothing rejected two. So `XPENDING k` — five keystrokes, no
authentication, no special state — killed a shard thread.

Every sibling in that file already refuses a short call in Redis's words:
XGROUP at `args.len() < 2`, XACK at `< 4`, XCLAIM at `< 7`. This one was
missed, and nothing drove it.

The declared arity was already right: `-3`, the same as redis 8.10.1
reports. The guard was simply absent.

## 2. `DEL` / `EXISTS` / `UNLINK` answered `:0` with no key

`cmd_resolve.rs` routed anything that was not exactly two arguments to the
multi-key fan-out, which summed zero targets — an empty delete reported as
success. redis 8.10.1 answers all three with the arity sentence.

`dispatch.rs` had the correct guard the whole time (`args.len() < 2 =>
wrong_args`). The router never let a keyless call reach it. Fixed by
routing a short call locally, which is what the default arm already does.

**The embedded facade mirrored the `:0` on purpose.** The comment there
said so:

> Bare DEL / UNLINK / EXISTS answer `:0` on the wire (the runtime's
> multi-key fan-out sums zero targets) — mirror that, not the
> dispatch-layer arity error the route never reaches.

That is the interesting part of this finding. The differential harness
holds the two surfaces byte-identical, and they WERE byte-identical: both
said `:0`. **Agreement on a wrong answer is still agreement**, so no
differential test could see it. It took an oracle outside both — a running
redis 8.10.1 — to say which of the two agreeing surfaces was right.

And when the server was fixed first, the harness caught the one-sided fix
on the next run, by name. That is the division of labour: the oracle says
what is correct, the differential says the two surfaces have not drifted.
Neither replaces the other.

## 3. `SRANDMEMBER` declared -3 where redis declares -2

The handler accepts `SRANDMEMBER key`; it is a valid call and it worked.
But `VERB_META` declared a minimum of three, and the MULTI queue checks the
declared minimum before queueing:

```
SRANDMEMBER k          ->  $-1        (accepted)
MULTI
SRANDMEMBER k          ->  -ERR wrong number of arguments for 'srandmember'
```

**The same command meant two different things depending on whether a
transaction was open**, and the cause was one wrong number in a
documentation table. Its own cell now holds this
(`a_valid_call_is_not_refused_by_the_transaction_queue`).

## 4. `XREAD a b` said "syntax error"

redis 8.10.1 answers `XREAD a b` with the arity sentence and keeps "syntax
error" for `XREAD a b c` — long enough to parse, shaped wrong. Both
verified against the container rather than from memory.

## What is left, and why it is registered rather than fixed

Eleven verbs answer a short call with their own usage line: FAILOVER,
REPL.WAIT, and the IDX. / TABLE. / VIEW. declaration verbs. Every one is
kevy-only — there is no Redis counterpart to be compatible with — and for
a verb that takes eleven arguments a usage line is the more useful answer
than "wrong number of arguments". They are named in `OWN_USAGE_LINE`, and
the ledger is exact in both directions: a verb that starts doing this
fails, and so does one that stops.

## The gate, and its red

- removing the XPENDING guard: the sweep fails with `index out of bounds`
- dropping TABLE.VERIFY from the ledger: `1 verb(s) answer a short call
  with neither the arity sentence nor a registered usage line`
- adding GET to the ledger: `["GET"] now answer in Redis's words`
- a floor refuses under 150 probed, since a sweep that probed nothing
  passes every assertion under it
- two controls: a genuinely unknown verb must stay unknown, and a correct
  call must not become an arity error

The hand list is gone. Keeping both would be the duplication this release
is about.

## The lesson worth keeping

A hand-written list is a hypothesis about where defects are. It is worth
writing when nothing else can enumerate the space — and worth deleting the
day something can. The thing that made the space enumerable here was not a
test-side idea; it was moving one column into a crate that steel is
allowed to read, for an unrelated reason.

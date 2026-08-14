# blpop_remote_disconnect_then_push_is_clean — first occurrence

**CI run 31752317736, attempt 1, x86_64-unknown-linux-gnu. Attempt 2 on
the same commit: green. Local: 8/8 green on macOS.**

```
assertion `left == right` failed: the gone waiter must not consume the
later push. *0 = it did (the defect escrow closes,
bench/FINDING-2026-07-19-xshard-block-serve-drop.md);
got: [42, 48, 13, 10]
crates/kevy/tests/blocking_cross_shard.rs:302
```

`[42, 48, 13, 10]` is `*0\r\n` — an empty array where the test expects the
pushed element. The waiter that had already disconnected consumed the
later push, which is the exact defect the escrow exists to prevent.

## Why this one is not filed as noise

It guards a data-loss path, not a timing convenience. The escrow was
built because a cross-shard blocking serve could lose the element it
popped; the fix ties release to the write RESULT rather than to a
point-in-time "is the connection alive", precisely because that guess can
go stale between the peek and the release.

And the failure shape matches the family's history. The finding records
that an intermediate fix "closed window 2 on kqueue but left a ~10%
residual on the epoll fallback". Local is macOS/kqueue; CI is
Linux/epoll. A test that passes locally and fails on CI is, for this
particular defect, weak evidence of flakiness and moderate evidence of a
residual — which is why the local 8/8 is recorded here as what it is
(the wrong reactor) rather than as an exoneration.

## What would settle it

The escrow's release path is observable: `serve_confirm[conn]`,
`confirm_serve_delivered`, `restore_serve_on_teardown`. A loop of this
test under forced epoll (`KEVY_IO_URING=0`) on a loaded Linux box, with
those three logged, distinguishes "the release raced the teardown" from
"the teardown never restored". Neither has been run — this note records a
first occurrence and the shape of the question, not an answer.

## Standing

First occurrence. No prior entry in this archive. Not blocking the branch
it appeared on (a site redesign and a benchmark re-measurement; nothing
in either touches the reactor, and the last change under crates/kevy-rt
was 2026-08-12). Recurrence promotes it to an instrumented-repro arc, the
way the availgate wedge was promoted after its third.

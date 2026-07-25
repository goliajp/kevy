# `subscribe()` returns before the subscription is live

**Status: open by design decision, not by neglect.** The three test-side
fixes below are landed; the API shape that invited them is not changed,
because changing it is a change to `docs/client-contract.md` — the spec
every language door implements.

## The shape

`Subscriber.subscribe()` writes `SUBSCRIBE` and returns. It does not wait
for the server's ack. `connect_channels()` is `connect()` + `subscribe()`,
so it returns while the subscription may not yet be registered
server-side.

A caller that subscribes and then publishes — from another connection, or
by prompting anything else to publish — is racing. The message goes to
whoever is registered at PUBLISH time. Losing it is silent: a blocking
`recv_message()` skips ack frames and waits for a message that will never
come, forever.

That the ack is the caller's to consume is visible in the suite itself —
`test_read_timeout_bounds_recv` reads it explicitly with
`sub.recv()  # subscribe ack`. So the contract is defensible as written.
It is also a footgun, and this is not a hypothesis about one:

## Three independent instances, one day

| Where | Cost |
|---|---|
| `bench/clientgate/node_redis.mjs` | Most of a day. The lost message stalled the smoke, and because the script has no `step()` marker after "SET", the log's last line was `step: SET` — which I read as "hangs at SET" and chased as an io_uring server wedge. It was the pub/sub step at the end. A repro harness was built and committed around that misreading before tcpdump showed the run reaching PUBLISH. |
| `bench/clientgate/redispy.py`, `redispy_async.py` | Latent. Same defect, never fired; found by sweeping the siblings after the first one. redis-py's `subscribe()` also only writes. |
| `bindings/python/tests/test_pubsub.py::test_connect_channels_and_recv_message[remote]` | The `client-conformance (python)` job sat in_progress for **3h46m** and had to be cancelled. Reproduced on real Linux at 2 failures in 20 suite runs (~10%). |

Three tests, written at different times by different hands, all raced the
same way. That is a property of the API, not of the tests.

Worth noting what made the third one findable: a per-test timeout. Without
it the only evidence was a job that never finished and a log whose last
line said 72 tests had passed. With it, pytest named the test in 60s. The
fix followed in minutes.

## Options, none of them local

- **`subscribe()` waits for its acks.** Best contract — "subscribed when
  it returns" — and removes the footgun for every caller. But the ack is
  currently delivered through `recv()`, so consuming it inside `subscribe()`
  changes the event stream users see. That is a breaking contract change
  across every door.
- **Add an explicit `subscribe_sync()` / `wait_subscribed()`.** Additive,
  no break, but leaves the sharp edge as the default.
- **Document it loudly and leave it.** Cheapest, and the evidence above
  suggests documentation does not prevent this — the contract already
  implies it and three tests still got it wrong.

## Related

`bench/FINDING-2026-07-19-xshard-block-serve-drop.md` is the other defect
found this session whose fix is a protocol decision rather than a patch.
Both belong in the same design round.

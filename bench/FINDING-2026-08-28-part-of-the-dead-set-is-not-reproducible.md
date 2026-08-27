# FINDING 2026-08-28 — part of the dead set is not reproducible, and the ratchet had to be told

**Status**: measured across two CI runs, five symbols registered as
unstable, the ratchet's tolerance now comes from the baseline.

## What deadgate reported

Its first two real runs both refused, and both refused over a set that had
mostly improved:

| run | dead regions | grew/joined | shrank | left |
|---|---:|---:|---:|---:|
| A (`6306ad31`) | 28,265 | 16 | 82 | 9 |
| B (`8b9f6134`) | 27,917 | 12 | 99 | 15 |

covgate passed both times — 82.94% then 83.18% against a 79.64% baseline.
**Coverage rose while named symbols went backwards**, which is the case a
scalar cannot express and the reason this gate stores identities.

## The comparison that settles it

Both runs judge against the same baseline, and the reactor and replication
symbols are untouched by the code that differs between them. So a symbol
that grows in one run and matches the baseline in the other has moved on its
own.

**Present in both, same delta** — consistently different from the baseline:
`Shard::close_conn` 4→5, `Shard::tick_replication_view` 12→13,
`Shard::tick_replication_watermark` 4→5, `Shard::unregister_subs` 1→2,
`::mark` joined at 10, `replica_runner::run_loop` joined at 6,
`replica_runner::sleep_interruptible` 9→15, `aof_writer::run_worker` 5→6.

**Present in one run only** — proven to vary:

| symbol | run A | run B |
|---|---|---|
| `Shard::deliver_pmessages` | 3 | 2 (matched baseline) |
| `Shard::try_inline_local` | 13 | 11 (matched baseline) |
| `Shard::do_publish` | 1 (matched baseline) | 4 |
| `::get_into_output` | 17 | 16 (matched baseline) |
| `::top_groups` | **23** | **12** (matched baseline) |

`top_groups` swings by eleven regions between two runs of the same corpus.

## Why, by reading the branches

Every one is scheduling-dependent. `unregister_subs` has a `None => false`
arm that turns on whether a channel is still registered when a connection
drops. `tick_replication_watermark` matches on each replica's state and
`_ => continue`s on the rest — which arm runs depends on where the state
machine is at tick time. `try_inline_local` takes its fast path only when
the target shard is this one and the inbox is empty. Whether these arms run
depends on where the loop happened to be when the last test ended.

## What could not be measured here, and was not claimed

Two local attempts failed and are recorded rather than rounded off. Running
kevy-rt's tests twice gave identical sets — but kevy-rt alone is 94.7% dead
because its own tests never drive the reactor, so the experiment was aimed
at the wrong target, and byte-identical output is as consistent with caching
as with determinism. Running the replication suite twice failed on the
second pass with "kevy ready timeout" on two tests: this machine cannot
start a server reliably under its own load, which cost four diagnoses this
session.

So the experiment moved to CI, where two runs exist and the runner is not
also compiling something else.

## The tolerance, and its bounds

`suite/dead-paths.toml` grows an `[[unstable]]` form. The five proven
symbols are registered with their differing values as evidence and a reason.
The ratchet reports them and does not fail on them.

That is a real loosening, bounded four ways:

1. **Evidence is required.** An entry without the differing values and a
   `why` is refused, not ignored.
2. **One symbol per entry**, never a crate — the crate-level form exists for
   a different thing and is not reusable here.
3. **The exemption comes from the baseline**, never from the observed set.
   A run cannot exempt itself; verified by a test where the observed side
   declares a symbol unstable and still fails.
4. **The eight consistently-different symbols are NOT registered.** They
   differ from the baseline in both runs, which is equally consistent with
   the baseline having sampled the lucky side — and that is fixed by
   re-recording the baseline, not by widening the tolerance. If they later
   vary between runs, they earn an entry with evidence like the others.

## One more thing the runs exposed

CI's `upload dead set` step was skipped on both failures, because a step
after a failed one does not run. The set the gate refused over is exactly
the evidence needed to judge the refusal, so it now uploads with
`if: always()`. Putting the evidence behind the verdict is a small mistake
that costs a whole cycle each time.

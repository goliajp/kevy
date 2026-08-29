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


---

## Continued — the tail did not converge, so the baseline became an envelope

The prefix registrations took the growth from seventeen items to two. But
the next two runs each named a *different* pair, with no overlap:

| baseline | run's growth |
|---|---|
| v1 (one sample) | `::match_migrating`, `::resolve_xadd_id` |
| v2 (one sample) | `kevy::replica_runner::maybe_ack`, `kevy::dispatch::dispatch_with_proto`, `kevy_alloc::os::trim` |

That is the condition the v1 acceptance reason named in advance: *"if
different symbols appear each run instead, that pattern is itself the
evidence that the tail of this measurement is noisy."*

Adding a prefix per subsystem would not converge either, because the next
run names something in a subsystem not yet listed. The tail is not confined
to one place; it is a few symbols per run, anywhere the corpus's timing
reaches.

### The discriminator is persistence, not size

A real regression — a deleted test, a new untested branch — persists across
runs and usually lands many regions in one symbol. Noise moves. A threshold
on the tail's *size* would be a number invented to make a gate quiet; the
property that actually separates the two is whether it repeats.

A single CI run cannot see two runs. What it can do is compare against a
baseline that already knows what the noise has been observed to do. So the
baseline is now an **element-wise maximum over three runs**:
`setratchet.py envelope <baseline> <run1> <run2> <run3>` records, per
symbol, the worst count any of them showed.

Growth then means **worse than the worst of N runs**, which is a claim about
the code rather than about which run it was.

  three runs: 27,858 / 27,738 / 27,788 dead regions
  envelope:   27,873 over 2,615 symbols

The envelope sits above every individual run because each symbol takes its
own maximum, not because any run was that bad.

### Verified both ways

All three runs pass against the envelope. An injected regression — one
symbol +50, one new symbol +30 — still fails, and it fails while the TOTAL
region count is *lower* than the baseline's (27,868 against 27,873). A real
regression hidden behind an overall improvement is precisely what a scalar
ratchet cannot see, and the per-symbol comparison catches it.

`envelope_runs` is recorded in the baseline, because an envelope over one
run is a sample and should not be able to pass itself off as more.

---

## Continuation, 2026-08-28: the tail is mostly not noise

The envelope over three runs held for a day, then two more symbols joined
the set on separate runs — one per run, each time while the *total* fell:

| run | symbol joined | regions | total |
|---|---|---:|---:|
| 33113251307 | `kevy::commands::commands_tick::sweep_hash_field_ttls` | 5 | 27,873 → 27,727 |
| 33118984761 | `kevy_elect::transport_loops::message_sender` | 3 | 27,873 → 27,721 |

The cheap reading is that the envelope needs more runs. The measured
reading is different, and better: **neither symbol is irreducibly
nondeterministic.** Both were reachable only *incidentally*, through
whatever a concurrent test happened to do inside its window, and both took
a direct test to pin:

- `sweep_hash_field_ttls` runs when a hash field's deadline falls due
  between two ticks. Every hash operation purges lazily on access, so a
  test that reads the expired field proves nothing — the read removes it
  either way. The deadline goes in through the snapshot loader hook, which
  skips the command path's immediate-delete branch, and the accounting is
  read with `hash_ttl_each`, which iterates without purging.
- `message_sender` is a pure four-arm match over the message variants. Its
  arms were covered only by whichever messages a real election exchanged
  inside the test window. Which variants those are is a matter of timing;
  which variants *exist* is not.

So the working rule is: **a symbol that joins the set gets read before it
gets declared.** Two of two so far turned out to be coverable, and a
ratchet that absorbs every symbol that ever varies converges on absorbing
everything. The four `unstable` prefixes already declared are all thread
loops — `Shard::`, `replica_runner::`, `wire_snapshot::`, `aof_writer::` —
and that remains the shape that earns a declaration: work that only exists
because another thread is running, not work that a test could ask for
directly.

### A third, and the rule holds

| run | symbol joined or grew | regions | total |
|---|---|---:|---:|
| 33125281621 | `kevy::cmd_block_serve::block_ready::` | 3 → 11 | 27,873 → 27,753 |

Read before declared, like the other two, and coverable like the other two —
a five-arm match over `BlockKind` whose arms the wider suite reaches only
when a blocked client of that kind is served inside a test's window.

Unlike the other two, asking it directly found something. Its XREAD arm's
condition was `!tmp.is_empty()` over a dispatched replay, and `XREAD` writes
`*-1\r\n` for "nothing new" — so the arm was always ready, while its comment
said "non-empty output ⇒ data is available". The test failed on the first
assertion, which is what a test for a symbol nobody executes is for.

Three of three now. The tail is not noise; it is work that only happens
because another thread happened to do something, and each time the direct
question has been askable. The four `unstable` prefixes remain the shape
that earns a declaration — thread loops, where the work exists only because
another thread is running.

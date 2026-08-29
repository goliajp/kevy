# Forty-five examples that nothing ran

*2026-08-28 — found while adding the first doctest to kevy-uring.*

## The claim

`bench/doctestgate.sh` measures what the documentation owes, and its own
header states the reason it prefers examples to prose:

> a paragraph is a promise nobody checked and a doctest is compiled and run

`suite/stone-waivers.toml` turns that into a rule every stone must meet:

> Prose is unverified; a doctest is compiled and run. 12 of 17 stones carry
> no executable example at all, so 1 is a floor to start a ratchet from

Both sentences rest on the same premise: that an example, unlike a
paragraph, is executed.

## The measurement

```
$ grep -rn -- "--doc\b" .github/workflows/*.yml suite/manifest.toml bench/*.sh tools/*.py
$
```

Nothing. The string `--doc` did not occur anywhere in this repository.

Every place tests are run spells it the same way:

| where | command |
|---|---|
| `.github/workflows/ci.yml:172` | `cargo test --workspace --lib --tests --target …` |
| `.github/workflows/ci.yml:366` | `cargo test --workspace --release --lib --tests` |
| `.github/workflows/release.yml:38` | `cargo test --workspace --release --lib --tests` |
| `suite/manifest.toml:359` | `cargo test --workspace --lib --tests` |

`--lib --tests` is not a shorthand for "the tests". It is a target filter,
and it is precisely the pair that **excludes** doctests. Passing no filter
runs them; naming any filter runs only what is named.

So the forty-five examples in this tree — including the eleven this arc
added specifically to satisfy the stone bar — had never been compiled or
run by CI, by the release workflow, or by the suite. Run by hand for the
first time today: 45 passed, 0 failed. They were fine. Nothing knew that.

## Why it survived

The gate that cares about examples counts them; the runner that could
execute them filters them out. Neither is wrong on its own terms, and the
sentence that joins them — "a doctest is compiled and run" — was true of
doctests in general and false of these.

This is the same shape as the arc's other findings: a green that answers
a different question. `doctestgate` was answering *how many items carry an
example*, and being read as *the examples work*.

## The fix

`bench/doctestrun.sh`, wired into CI on the Linux target and into the
suite's prerelease tier. It runs `cargo test --workspace --doc`, and the
guard around the run matters more than the run:

- A doctest harness that collects nothing exits 0 and prints `0 passed` —
  the exact shape of a clean bill of health. An empty collection is a
  REFUSAL, not a pass.
- The collected count is checked against a witness sharing no machinery
  with cargo: an awk pass that pairs fences per file and counts only the
  tags rustdoc will build (bare, `rust`, `no_run`, `should_panic`,
  `compile_fail`, `edition*`), excluding this tree's 55 ```text diagrams
  and 6 shell transcripts. Witness: 49. Collected on macOS: 45, the
  difference being the Linux-only crates.
- The floor is three quarters of the witness, not equality, because a
  platform-gated crate legitimately contributes fences the host cannot
  compile. It catches the collector going dark; it does not police a ratio.

## A bug in the gate, caught by testing the gate

The first version read the summary line positionally:

```sh
FAILED=$(grep '^test result' "$LOG" | awk -F'[ ;]' '{f+=$6} END{print f+0}')
```

Splitting `test result: ok. 45 passed; 3 failed; …` on `[ ;]` puts an
**empty** field at `$6` — the semicolon and the space each end a field —
and the failure count lands at `$7`. That version reported zero failures
however many there were. It was caught by injecting a failing example
rather than by reading the code, and it is why the script now matches the
number by name.

The verdict order was wrong too: a failure in an early crate makes cargo
abandon the rest, so the collected count drops, and a floor checked first
would have reported a broken apparatus for a broken example. The run's own
verdict is now read before anything is concluded about how much of it ran.

Red-green, both directions:

```
$ bash bench/doctestrun.sh
doctestrun: PASS — 45 examples compiled and ran (witness: 49 runnable fences)
$ # inject `assert_eq!(2 + 2, 5);` as a doctest in kevy-tmpdir
$ bash bench/doctestrun.sh
doctestrun: FAIL — 1 of 44 examples do not run
---- crates/kevy-tmpdir/src/lib.rs - unique_dir (line 43) stdout ----
```

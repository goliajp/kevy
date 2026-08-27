# RFC — v6 toolchain: the instruments, gauges, and fixtures the goals require

Status: **Phase A — design only.** No device is built by this document.

v6's charter (owner, 2026-08-23): a clean architecture; the stones made
solid; code quality, documentation and performance pushed to their current
limit; **no dead code and no dead paths**; **no complex implementation where
a simpler one delivers the same capability**.

This RFC designs the measuring equipment *before* the milestone, because
three of those five predicates cannot be evaluated by anything this
repository currently owns.

---

## 1. What the existing wall measures — and the shape it cannot express

The wall is large and, within its shape, good: 102 gate scripts under
`bench/`, 25 checkers under `tools/`, a 20-check `suite precommit`, and four
recorded baselines.

**Every one of those four baselines is a scalar with a tolerance band.**

| baseline | what it stores |
|---|---|
| `COV-BASELINE.json` | `workspace_line_coverage_pct: 79.64` |
| `PERF-BASELINE.json` | throughput figures + `tolerance` |
| `MEM-BASELINE.json` | bytes-per-entry + `band_pct` |
| `DISK-BASELINE.json` | AOF bytes/op, rewrite ms + `band_pct` |

That is the right instrument for a quantity. It is the wrong instrument for
v6, because v6's predicates are not quantities:

- **"no dead paths"** is a statement about a **set** — *which* regions never
  execute. A scalar ratchet cannot see it: coverage can sit at 79.64 % for a
  year while the identity of the uncovered 20.36 % churns completely. The
  gate stays green through a total substitution of what is untested.
- **"no redundant implementation"** is a statement about a **relation** —
  *this* region delivers what *that* one delivers. No baseline in this
  repository stores a relation between two places in the code.
- **"stones made solid"** is a statement about a crate's **independence** —
  whether it stands up outside this workspace. `check_architecture.py`
  (5.3) checks dependency *direction*, which is necessary and not
  sufficient: a crate can point the right way and still be unliftable.

So the v6 toolchain is not "more gates". It is a **new kind of gate** — from
scalars to sets and relations — plus the fixtures that make a set or a
relation observable in the first place.

---

## 2. The measurements that size this work (taken 2026-08-23)

Nothing below is estimated. Each figure is the reason a device exists; a
device with no figure under it is not in this RFC.

| measured | figure | what it means for v6 |
|---|---:|---|
| workspace line coverage (Linux CI baseline) | **79.64 %** | ~1/5 of the code never executes, and **no artefact names which fifth** |
| public items across 48 crate doc-tables | **2,661** | `dead_code` **cannot see a single one** — the lint is defeated by `pub` in a lib crate |
| documented public items | **2,500 (93.9 %)** | prose docs are in good shape; this is *not* where the doc work is |
| public items carrying an executable example | **18 (0.7 %)** | **22 crates are "100 % documented, 0 examples"** — the documentation is almost entirely unverified |
| `#[allow(dead_code)]` sites in `crates/` | **26** | explicit escape hatches, currently unregistered |
| `#[allow(unreachable_code)]` sites | **2** | dead paths admitted in source |
| `#[allow(unused_imports)]` sites | **7** | same |
| non-`kevy` direct dependencies | **5** (`tokio`, `smol`, `async-std` in `kevy-client-async`; `loom` under `cfg(loom)`; `luna-core` in `kevy-lua`) | each defensible; **none declared anywhere a machine can check** — the 0-dep constraint lives only in prose |
| crates classified in `suite/architecture.toml` | 17 stone / 16 steel / 10 cement / 4 support | the stone list is v6's primary work surface |

Two of these deserve to be stated plainly because they invert the intuition:

**The documentation problem is not coverage.** At 93.9 % prose coverage,
writing more prose is close to finished work. At **0.7 % executable
examples**, essentially nothing in the documentation is *checked against the
code*. A doctest is compiled and run; a paragraph is not. For a stone —
widest blast radius, promised to any project that takes it — an undoctested
public function is an unverified promise.

**The dead-code problem is not the lint.** `[workspace.lints.rust] warnings
= "deny"` already makes an unreferenced private item a compile error. That
half is closed. The open half is that **`pub` in a library crate exempts an
item from `dead_code` entirely**, and there are 2,661 such items. Whatever
finds dead public surface, it will not be rustc.

---

## 3. Three classes of device

Deliberate separation, because conflating them is how a gate ends up
asserting something nobody measured:

- **仪器 / instrument** — reveals something invisible and produces a
  **description**. Never a verdict. Run by a human or a job; read, argued
  with, and used to decide whether a gauge is warranted.
- **量具 / gauge** — compares an observation against a **recorded
  expectation** and produces a verdict. Runs in `suite` / CI.
- **夹具 / fixture** — holds the work steady so the measurement is
  repeatable, or possible at all. No verdict, no description; it is the
  bench the other two stand on.

### 3.1 仪器 — instruments

**I1 · zero-hit region atlas.** `cargo llvm-cov --json` already emits
per-function region hit counts; the atlas is the missing consumer. Output:
every region with `count == 0`, keyed by **symbol**, with the source
excerpt. Then — and this is the part that makes it a v6 device rather than a
report — it **partitions** the dead set into four classes:

  1. *reachable, untested* → a test is owed
  2. *unreachable by construction* → the code is owed a deletion, or a proof
  3. *platform-gated* → owed a measurement on that platform
  4. *panic/abort paths* → declared, and excluded by rule rather than by drift

The partition is the actual v6 work list. The atlas is what makes it
enumerable instead of rhetorical.

**I2 · clone atlas.** Winnowing (Schleimer–Wilkerson–Aiken) over a
normalised token stream: strip comments and layout, k-gram hash, keep the
minimum hash per window, report regions sharing fingerprints. Pure Python,
zero dependencies, orthodox algorithm. Output: ranked pairs of similar
regions, **cross-crate pairs first**, since two crates solving one problem
twice is the shape v6 is hunting.

**I3 · public-surface reachability graph.** For each of the 2,661 public
items: is it reached from a test, a doctest, a bench, or another crate in
this workspace? An item reached by nothing is not automatically dead — a
published crate's API legitimately serves callers outside this tree — so the
instrument reports three states (*reached in-tree*, *reached only by its own
tests*, *reached by nothing*) and never converts them to a verdict on its
own. The third state is where dead public surface hides.

**I4 · stone extraction report.** Per stone: does it build and test with
its own `Cargo.toml` in a bare directory outside this workspace? Does
`cargo-semver-checks` (installed) report its public API compatible with the
last published version? What is its doc coverage, example coverage, and
zero-hit set? One row per stone, 17 rows.

### 3.2 量具 — gauges

**G1 · the set-ratchet.** The central new mechanism, and the one piece of
genuine invention here. A scalar ratchet stores a number and asks "did it
get worse". A **set-ratchet stores identities** and asks "did anything new
appear".

The design problem is that line numbers are not identities — one edit
shifts a file and the whole set churns, and a baseline that churns is a
baseline nobody trusts. So the recorded identity is **the symbol, with a
count**:

```json
{"kevy-store::tier_demote::demote_one": 3}
```

— three never-executed regions inside that function. The rule: **no symbol
may gain regions, and no symbol may join the set.** Stable under
reformatting and under edits elsewhere in the file; moves when the function
is renamed, which is a rename the author should be declaring anyway;
diffable in review, so the baseline's *change* is reviewable rather than
just its size.

This mechanism is what G2 and G5 are both built from. It is worth building
once, properly, as `tools/setratchet.py`.

**G2 · deadgate** — the never-executed **set** may only shrink. Built on I1
+ G1. This is the gate the "no dead paths" goal actually names, and the
single largest lever in the RFC: it converts an unowned 20.36 % into a
list with an owner per line.

**G3 · stonegate** — a per-stone scorecard with thresholds, built on I4.
Proposed initial bar, to be set from the first I4 run rather than from
taste: builds standalone; semver-clean against the published version; doc
coverage 100 %; **every public item carries an executable example**;
zero-hit set empty or explicitly registered. Stones only — 17 crates, the
widest blast radius, which is where the methodology says the effort goes.

**G4 · depgate** — every non-`kevy` dependency must be declared in an
allowlist **with a reason**, per crate, per dependency kind. An undeclared
one fails. This costs an afternoon and closes a constraint that has been
load-bearing prose since L2. Cheap, so it goes first.

**G5 · doctestgate** — a floor on executable examples, ratcheted per crate
via G1. Stones to 100 % under G3; everything else forbidden to regress.

### 3.3 夹具 — fixtures

**F1 · the extraction sandbox.** Copies a stone and its stone-only
dependency closure into a bare temp workspace and builds and tests it there.
This is not a convenience — it is the **operational definition of a stone**
("any project could take these"), which until now has been an assertion. A
stone that cannot be lifted is not a stone, and this is the only device that
can say so.

**F2 · the differential harness.** Where two implementations serve one
capability *by design* — the epoll and io_uring reactors, the packed and
general row, `kevy-alloc` and the system allocator — drive both with one
command corpus and assert the **observable behaviour is identical**: the
RESP response byte stream, and the persisted state after replay. Existing
parts to build on rather than duplicate: `kevy-testnet`, `compat3.sh`,
`dialectgate.sh`.

This fixture is what turns "can one of these be deleted?" from an argument
into an experiment. It is also the honest answer to the goal's hard case:
some duplication is deliberate and correct (a portable path beside a fast
one), and the harness is what distinguishes deliberate duplication —
provably equivalent, separately justified — from accidental duplication.

**F3 · the execution corpus.** "Never executed" is meaningless without
naming what did the executing. The corpus is declared once and fixed: unit
tests + integration tests + doctests + the bench smoke paths, at a pinned
seed, on the enforcing platform (Linux CI, as `COV-BASELINE.json` already
records). Changing the corpus invalidates the deadgate baseline **by
construction** — the baseline records the corpus's identity alongside the
set.

---

## 4. Standing rules for every device in this RFC

Each of these is scar tissue from this repository. They are cheaper to
honour at design time than to learn again.

1. **Refuse rather than pass vacuously.** A device that cannot see its
   subject exits non-zero with a distinct code. `secretgate` and
   `check_architecture.py` already do this ("finding zero crates is a
   failure of the selector, not a pass"). Every new device inherits it.
2. **Every measurement carries a witness unrelated to the measured
   quantity.** A failed measuring device produces output shaped exactly
   like data. The doc-coverage figure in §2 was computed twice today; the
   first attempt returned no rows and only crashed because nothing caught
   the division — had it defaulted, it would have reported a confident
   0.0 %.
3. **A set-ratchet stores identities, never counts.** A count is a scalar
   wearing a set's clothes and permits exactly the substitution this RFC
   exists to prevent.
4. **The exit code is judged alone and emitted last.** No `cmd ; cleanup`
   where the reported status is the cleanup's.
5. **An instrument is never promoted to a gauge in the same change that
   introduces it.** Run it, read what it says, *then* decide whether a
   threshold is warranted and where it sits.

---

## 5. Build order

Ordered by what each unlocks, not by size.

| # | device | why here | depends on |
|---|---|---|---|
| 1 | **G4 depgate** | cheapest; converts a prose constraint into a machine check; independent of everything | — |
| 2 | **F3 execution corpus** | nothing about dead paths means anything until "executed by what" is fixed | — |
| 3 | **I1 zero-hit atlas** | produces the v6 work list — the largest single deliverable in this RFC | F3 |
| 4 | **G1 set-ratchet** | the mechanism G2 and G5 are both made of; built once, properly | — |
| 5 | **G2 deadgate** | locks in every metre the I1 partition gains | I1, G1, F3 |
| 6 | **F1 extraction sandbox** | makes "stone" operational | — |
| 7 | **I4 stone report** | 17 rows; sets G3's bar from data instead of taste | F1 |
| 8 | **G3 stonegate** | the stones goal, enforced | I4, F1 |
| 9 | **G5 doctestgate** | 0.7 % → a floor that rises | G1 |
| 10 | **I2 clone atlas** | see §6 — read before anything is designed on top of it | — |
| 11 | **F2 differential harness** | the honest instrument for "same capability, simpler implementation" | — |

Steps 1–2 and 4 and 6 are independent and can proceed in parallel.

---

## 6. What must be measured before it can be designed

**The redundancy goal has no gate in this RFC, deliberately.**

"No complex implementation where a simpler one delivers the same capability"
is the goal I can size least. I have not measured whether this repository
*has* meaningful duplication — 102 gate scripts and 47 crates make it
plausible, and plausible is exactly the standard this project's own
methodology forbids building on. The Pre-Phase-B rule was written for
performance work, but it generalises without modification: *an attack whose
target has not been shown to be a double-digit share of the total is
hand-waving.*

So: **I2 runs first and its output is read before any dedup gate is
designed.** If the atlas shows the duplication is small or entirely
deliberate, the correct v6 outcome is a documented register of intentional
twins — validated by F2 — and **no gate at all**. That is a real possible
answer and this RFC declines to pre-commit against it.

The same restraint applies to a complexity gauge (cyclomatic complexity,
nesting depth). `locgate` and `funcgate` already bound file and function
size. Nothing measured says complexity is a live defect here, so nothing in
this RFC proposes measuring it. If the clone atlas or the I1 partition turns
up complexity as a cause, it earns a device then.

---

## 7. What this RFC does not decide

- **The thresholds.** G3's bar is set from I4's first run; G2's and G5's
  baselines are whatever the first honest run records. A threshold chosen
  before the first measurement is taste wearing a gauge's clothes.
- **Whether every stone can reach 100 % examples.** Seventeen crates, 0.7 %
  today. The measurement comes first.
- **Whether dead paths can reach zero.** Some of the 20.36 % is
  platform-gated and some is panic-path. The I1 partition is what makes the
  achievable target statable; until it runs, "zero dead paths" is a slogan.

---

## 8. Owner decision requested

The vision is the owner's. What this RFC asks:

1. **The shift from scalar baselines to set-ratchets** (§3.2 G1) is the
   central design commitment. Everything about dead paths follows from it.
2. **Stones-only for G3** — the strictest bar applies to 17 of 47 crates,
   per the methodology's effort-by-blast-radius rule, rather than
   workspace-wide.
3. **No dedup gate until I2 has been read** (§6), accepting "a register of
   deliberate twins and no gate" as a legitimate v6 outcome.

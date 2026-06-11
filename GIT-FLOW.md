# kevy git-flow SOP

The branching model is the classic nvie git-flow (master = stable +
tagged, develop = integration, feature/release/hotfix from there),
but the **conventions on top of it** are tuned for single-author
autorun mode: minimal ceremony, no PR review tax, CI as the only gate.

## One-time per-clone setup

```bash
bash .githooks/install.sh
```

Wires `core.hooksPath = .githooks` (pre-commit LOC check), squash-on-
finish, and fetch-before-finish for both feature and release. Re-run
after pulling a change that touches `.githooks/`. See the script for
the exact `git config` lines if you'd rather set them by hand.

## Branches

| Branch          | Role                                       | Notes                                                                                |
| --------------- | ------------------------------------------ | ------------------------------------------------------------------------------------ |
| `master`        | Stable release line. Every `vX.Y.Z` tag points here. | Never commit directly. Receives merges from `release/*` and `hotfix/*` only.         |
| `develop`       | Integration. Default branch on GitHub.      | Receives squash-merges from `feature/*`. Direct commits are reserved for trivial CI/docs touch-ups. |
| `feature/<name>`| Single self-contained piece of work.        | Forks from `develop`. Squash-merges back. Free-form local commits.                   |
| `release/<ver>` | Stabilization line for `vX.Y.Z`.           | Forks from `develop`. Holds version-bump + CHANGELOG only — no new features.         |
| `hotfix/<name>` | Emergency fix on the **published** line.   | Forks from `master`. Merges back to both `master` and `develop`.                     |

## Feature flow — the everyday case

For one self-contained sprint (a single coherent change set):

```bash
# Start
git flow feature start v2-7d-block-reactor

# Hack — N local commits are fine; commit early and often
…
git commit -m "feat(kevy-rt): blocked-client registry"
git commit -m "feat(kevy): BLPOP / BRPOP"
…

# When done locally: push once, let CI verify the whole thing
git push -u origin feature/v2-7d-block-reactor
gh run watch <id>          # wait for green

# Finish — squashes N commits → 1 commit on develop
git flow feature finish    # squash + delete remote feature branch
git push origin develop
```

**Rules learned from v2-7d retro**

- **Push the feature branch exactly once**, just before `finish` — not
  after every sub-commit. CI takes ~4 min per run; N pushes × 4 min
  buys you almost zero extra signal (sub-commit #1 is rarely tested
  by anything except sub-commit #N's tests anyway) and inflates flaky-
  test exposure. Source: every sub-commit of v2-7d pushed CI; only
  the final one's tests would have caught real regressions.
- **Don't pre-announce trait surface** in a stub sub-commit. If a
  trait method or struct field will be needed by sub-commit #2, it's
  almost always cleaner to ship them together in sub-commit #2 than
  to land a "stub registry, callers TBD" sub-commit #1.
- **`git flow feature finish` defaults to `--squash`** (set in the
  per-clone config). Sub-commits stay reachable on the feature branch
  reflog until GC; the squash carries a single conventional-commit
  message summarizing the whole sprint. Pass `--no-squash` only when
  the sprint genuinely has independently revertable sub-commits.
- **CI must be green on the feature branch before `finish`**.
  `gh run watch` blocks; don't merge red. The only exception is a
  known-flaky test documented in memory or `.claude/known-flakes.md`
  (re-run once; if it still fails, treat as real).

## Perf gate — mandatory before finish and release

Throughput regressions are release blockers. Before `git flow feature
finish` on any branch that touches `crates/` hot paths, and always on
`release/*` before finishing:

```sh
rsync -a --delete crates/ lx64:~/kevy-bench/crates/
rsync -a Cargo.toml Cargo.lock bench/perfgate.sh lx64:~/kevy-bench/
ssh lx64 'cd ~/kevy-bench && RUSTC_WRAPPER= cargo build --release -p kevy \
  && cp target/release/kevy /tmp/kevy_gate \
  && bash perfgate.sh /tmp/kevy_gate'
```

Exit 0 = PASS. Exit 1 = regression — do not finish, do not release; find
the regression first. Exit 2 = refused (dirty box / busy) — sweep or wait,
then rerun; never "just skip" the gate. The baseline lives in
`bench/PERF-BASELINE.json`; raise it (`--update-baseline` + commit) only
when a deliberate improvement lands, never to make a red gate green.

Methodology (since 2026-06-12): each angle measures **3 fresh server
instances** and gates on the median across them — instance-to-instance
spread (page placement / IRQ luck at server start, ±5 % on the 8sh
angles) dominates round-to-round noise (±2 %), so re-rolling rounds
against one instance just re-samples one draw. If the gate reds on a
single angle near the floor, first compare the *reference* binary
(`/tmp/kevy_gate`) on the same box state before suspecting the code; a
red on both is box state or methodology, not a regression. Preflight's
instantaneous idle check passes through IO-bound interference — eyeball
`uptime` (load < 1) before trusting a result.

## Release flow — stabilize → tag → publish

A release is a `vX.Y.Z` tag on `master` with the matching workspace
version bumps already in `Cargo.toml`. Pre-release polish lives on a
short-lived `release/*` branch so develop stays open for the next
sprint's features.

### Pick the version

- `vX.Y.Z+1` (patch) — bug fixes only, no behavior changes.
- `vX.Y+1.0` (minor) — new commands, new optional trait methods with
  defaults, additive config flags, new dependent crates.
- `vX+1.0.0` (major) — any breaking change to a `pub` item in any
  crate. Adding fields to a `pub struct` (e.g. `ResolvedCmd`) is
  breaking for external `impl Commands for X` users; in kevy's case
  KevyCommands is the only impl in-tree, so the effective audience
  is "downstream crates that pin `kevy-rt`" — judge accordingly. As
  long as `kevy-rt < 1.0` we treat such additions as minor.

### Do the bump

```bash
# Read RELEASE.md for the workspace vs per-crate routing decision
# (Route A: workspace-wide bump → one tag; Route B: per-crate dispatch).
# This SOP covers Route A; Route B is independent of git-flow.

git flow release start v1.4.0  # forks from develop

# 1. Bump [workspace.package].version in Cargo.toml.
# 2. Bump kevy-client / kevy-embedded's own version if they ship in
#    this release (their versions aren't workspace-inherited).
# 3. Update CHANGELOG.md — one section per release, newest on top.
# 4. cargo test --workspace --lib --tests   (the gate the release
#    actions also run — fail fast locally).

git add Cargo.toml CHANGELOG.md crates/kevy-client/Cargo.toml crates/kevy-embedded/Cargo.toml
git commit -m "chore(release): bump workspace to v1.4.0"

# Push to let CI verify the bump
git push -u origin release/v1.4.0
gh run watch <id>              # green
```

### Finish + publish

```bash
git flow release finish v1.4.0
# git-flow will:
#  - merge release/v1.4.0 → master (no-ff)
#  - tag v1.4.0 on master
#  - merge release/v1.4.0 → develop (back-merge)
#  - delete release branch (locally + remote)

# Push every reference the workflow needs to fire
git push origin master
git push origin develop
git push origin v1.4.0          # ← this triggers release.yml on GHA

# The release.yml workflow now runs:
#   verify → publish 16 crates (topo order, idempotent skip)
#   → build 3-target server binaries → draft GitHub release
# Monitor it; finalize the draft release notes on GitHub when done.
```

**Rules**

- **Bump versions on the release branch, not on develop.** This keeps
  develop's history free of "chore(release):" noise and lets a release
  abort cleanly (just delete the release branch, nothing on master /
  develop to revert).
- **CHANGELOG is part of the release commit**, written before tag.
- **Crates.io publish is irreversible** (only yank, never unpublish).
  Always run `release.yml` once with `dry_run = true` for any major
  bump before pushing the tag. Patch / minor on a tested branch can
  go direct.
- **Tag push triggers the workflow**. If the tag exists but `release.
  yml` didn't fire, check that the tag was pushed (`git push origin
  vX.Y.Z`), not just created locally.

## Hotfix flow — emergency fix on master

When a bug exists in a **published** version (i.e. it's reachable from
`master`, not just `develop`), and waiting for the next regular
release isn't acceptable:

```bash
git flow hotfix start fix-prod-rename-data-loss

# Fix only the bug. No refactors, no incidental cleanups, no version
# bump in unrelated crates.
# Bump patch version: vX.Y.Z → vX.Y.Z+1 on the same files as a release.
# Commit + push, watch CI.

git flow hotfix finish fix-prod-rename-data-loss
# Merges hotfix → master (tags vX.Y.Z+1) and hotfix → develop.

git push origin master develop vX.Y.Z+1
```

A bug that only exists on develop (e.g. v2-7c's io_uring Linux build
that never shipped) is **not** a hotfix — it's a regular feature/bugfix
branch off develop. The hotfix flow is for production-shipped issues.

## When to skip the flow

A handful of edits are too small to warrant a branch:

- One-line config / `.github/workflows/` tweak.
- Memory / `.claude/*.md` doc edits.
- README typo.

These can go straight to develop. Everything else gets its own branch.
The cost of `git flow feature start` is one command; the benefit (CI
gate, squash, traceability) is real.

## Anti-patterns observed in retros

| Anti-pattern                                           | Cost                                                                                                  | Fix                                                          |
| ------------------------------------------------------ | ----------------------------------------------------------------------------------------------------- | ------------------------------------------------------------ |
| `feature finish` without first pushing + waiting for CI | A broken Linux path lands on develop, breaks the next feature's baseline.                            | `git push -u origin feature/<name> && gh run watch <id>`.    |
| Sub-commit pushed individually, each with its own CI run | 4× CI minutes, 4× flaky-test exposure, no extra signal — only the last commit's tests catch regressions. | One push per feature; `finish` after green.                  |
| Bumping versions on develop                            | Mixes release admin into feature history; can't abort cleanly.                                       | Always `git flow release start`.                             |
| Treating a develop-only bug as a hotfix                | Bumps a patch version on master that has no real production fix. Pollutes the release history.       | Feature branch off develop. Reserve hotfix for shipped code. |
| Letting `auto_aof_rewrite_fires` (etc.) stay flaky      | One sprint's CI runs = N flaky-hit chances. The probability stacks.                                  | Treat flakies as fix-now bugs, not "known issues" forever.   |

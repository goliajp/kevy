# kevy cement audit — internal-correctness bar

> Companion to [`STONE-AUDIT.md`](STONE-AUDIT.md). Cement is project-glue
> code — not generic, not publishable, only useful inside kevy. Stone audit
> doesn't apply (no need for one-sentence identity, no API stability
> contract for outside callers, no crates.io publish prep). What we DO
> still require: cement must be correct, must respect the charter, and
> must not silently leak its concerns into other crates.

kevy's cements (5):

| Cement | Identity needs "for kevy"? | Why cement |
|---|---|---|
| `kevy-sys` | yes — "hand-curated libc bindings for kevy" | OS-boundary containment; nobody else would want this exact subset |
| `kevy-store` | yes — "Redis-semantic keyspace + value types" | Tied to Redis command surface + kevy's Value/Entry types |
| `kevy-persist` | yes — "RDB snapshots + AOF over `kevy_store::Value`" | Couples to kevy-store's internal types |
| `kevy-rt` | yes — "kevy thread-per-core shared-nothing runtime" | Specific reactor + Commands trait + connection lifecycle for kevy |
| `kevy` | yes — server binary | Top-level integration |

Dev tools (`kevy-bench`, `kevy-pubsub-bench`, `kevy-cli`) are NOT cement and
not subject to this audit — they go through the lightest gate (build, run,
no regressions). Their charter exemption status: each new dev tool must be
reviewed before being added.

---

## Tier system

Same 3-tier shape as STONE-AUDIT, but criteria are different:

- **T1 (GATE)** — break = production-blocker
- **T2 (BAR)** — break = blocks merge to develop
- **T3 (REVIEW)** — break = flagged for next refactor wave (no
  publish-equivalent gate, since cement isn't published)

---

## 1. Correctness baseline

| Layer | Criterion | Why |
|---|---|---|
| **T1** | `cargo test -p <name>` 100% pass | trivially required |
| **T1** | `cargo clippy -p <name> --all-targets -- -D warnings` 0 | hot-path correctness |
| **T1** | `cargo doc -p <name>` 0 warnings | doc rot is a cement smell |
| **T1** | unsafe scope justified at crate top + every `unsafe { ... }` has SAFETY: | same as stone |
| **T2** | Line coverage ≥ **80%** (cement bar, vs stone's 90%) | cement gets integration test coverage from kevy's e2e suite; lower threshold lets us count those |
| **T2** | Sharded integration tests pass on both reactors (epoll + io_uring) | most cement is reactor-touching |

## 2. Charter alignment

| Layer | Criterion | Why |
|---|---|---|
| **T1** | 0 third-party (crates.io) deps; only path-deps to other kevy crates | hard charter constraint |
| **T1** | `extern "C"` / libc only in `kevy-sys` (grep elsewhere = build break) | OS-boundary containment |
| **T1** | `forbid(unsafe_code)` where structurally possible (every cement crate that can; only kevy-sys + kevy-rt are exceptions today) | scope minimisation |
| **T2** | Workspace fields inherited (`version.workspace = true` etc.) | no per-cement policy drift |
| **T2** | No dep cycles | enforced by cargo, but include in audit checklist anyway |

## 3. Surface containment

| Layer | Criterion | Why |
|---|---|---|
| **T1** | Public surface kept tight (every `pub` crossing crate boundary justified by a real call site in another kevy crate) | cement should not accidentally become a stone |
| **T2** | Identity attempt: write one sentence describing what this cement does. If the sentence comes out clean + generic, suspect a hidden stone — split it out (this is how kevy-resp-client got carved out of kevy-cli). | continuously re-test the stone/cement boundary |

## 4. Performance baseline (only where it matters)

| Layer | Criterion | Where it applies |
|---|---|---|
| **T1** | If the cement is on the per-command hot path, perf flat from a recent A/B has been captured | currently: kevy-store, kevy-rt |
| **T2** | `BUDGETS.md` per cement that owns hot path | kevy-store / kevy-rt only |

`kevy-sys` / `kevy-persist` / `kevy` (the binary) don't carry perf
budgets here (kevy-sys is one-syscall-per-op below; persist is bounded
by disk; kevy is just main + serve glue).

## 5. kevy-sys specifics (OS-boundary cement)

Extra rules just for kevy-sys, since it's the only crate touching `extern "C"`:

| Layer | Criterion |
|---|---|
| **T1** | Every `extern "C"` fn signature matches the platform man page (POSIX / Linux / BSD as applicable; cited inline). |
| **T1** | Every `unsafe { ffi::… }` site has SAFETY: documenting the preconditions the kernel requires. |
| **T1** | `grep extern '"C"' crates/*/src` returns only kevy-sys hits. |
| **T2** | Linux + macOS code paths symmetric (every Socket method / poll/event type has cfg branches for both). |
| **T2** | miri / loom NOT REQUIRED (syscalls bypass both). The compensating audit is line-by-line review of FFI sigs vs man pages, captured in `crates/kevy-sys/AUDIT-2026-05-26.md`. |

---

## Audit verdict format

Same as STONE-AUDIT, written to `crates/<name>/AUDIT-2026-05-26.md`:

```markdown
# <name> cement audit — YYYY-MM-DD
## T1 (GATE)
- [x] cargo test: pass
- [x] clippy: 0
- [x] 0 third-party deps
- ...
## T2 (BAR)
- ...
## T3 (REVIEW)
- ...
## Verdict
GATE: ✅; BAR: ⚠️ (cov 76%, target 80%); REVIEW: ...
```

---

## How this differs from STONE-AUDIT

| Dimension | STONE | CEMENT |
|---|---|---|
| Identity test | required (≤60 char one sentence) | not required (cement is naturally project-flavored) |
| Coverage threshold | ≥ 90% lines | ≥ 80% lines |
| `cargo package --list` size budget | yes | no (not published) |
| `BUDGETS.md` / `MEM-BUDGETS.md` | required | only for hot-path cement (store, rt) |
| `README.md` / `CHANGELOG.md` / `PLATFORMS.md` | required (T3 PUB) | not required (no publish) |
| fuzz on parsers | required if parser | not required (kevy-resp covers the published parser; cement parsers are kevy-internal) |
| miri | required on unsafe | not required (esp. for syscall-bound code); explicit FFI review instead |
| Public surface review | minimal/audited | tight (don't grow accidentally) |

The cement bar is "do your job; don't leak your concerns; keep up with
the project's charter". The stone bar is "be publishable".

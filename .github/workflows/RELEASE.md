# Releasing kevy crates

Two routes to crates.io, one for coordinated workspace bumps and one
for independent per-crate cadence. Pick the one that matches the
shape of what you're shipping.

## Route A — workspace-wide (`release.yml`)

**When**: a workspace `version` bump that lands across all 14
workspace-inheriting crates at once (e.g. v1.0.4 → v1.1.0 for the
Borrowed-Argv chain), OR a coordinated release where you want the
whole shipping fleet to move together.

**How**:

1. Bump `[workspace.package] version = "x.y.z"` in the root
   `Cargo.toml` (plus any in-tree `version = "x.y.z"` dep pins).
2. Bump `kevy-client` and `kevy-embedded`'s **own** `version =`
   in their `Cargo.toml` if they're moving in the same release.
3. Commit + push to `develop`.
4. Tag `vX.Y.Z` and push the tag: `git tag v1.1.0 && git push origin v1.1.0`.
5. GHA `Release` workflow runs verify → publish (16-crate
   topologically-ordered chain with idempotent skip) → build server
   binaries for 3 targets → draft GitHub release.

Tag scheme: `v<workspace-version>` (e.g. `v1.1.0`).

## Route B — per-crate dispatch (`release-<crate>.yml` → `release-crate.yml`)

**When**: one crate ships an independent minor/patch and the rest of
the workspace should stay quiet — e.g. `kevy-client v1.4.0`
(multi-key + scan/keys + MULTI/EXEC) shipped without bumping anything
else, and `kevy-client v1.5.0` (typed Transaction builders + WATCH)
ships the same way.

**How**:

1. Bump the crate's own `version =` in its `Cargo.toml`. For
   workspace-inheriting crates, switch to an explicit `version = "x.y.z"`
   first if you need a cadence different from the workspace.
2. Commit + push to `develop`.
3. Actions tab → **"Release \<crate-name\>"** workflow → **Run
   workflow**. Pick `dry_run = true` for a dress rehearsal
   (build + test + `cargo publish --dry-run`, no tag, no publish).
   Re-run with `dry_run = false` for the real ship.
4. Workflow: resolves version from `Cargo.toml` (via `cargo
   metadata`, so `version.workspace = true` resolves correctly) →
   refuses if `<crate>-v<version>` tag already exists → build
   --release → test --release → `cargo publish` (idempotent: treats
   "already uploaded" as success, backfilling a per-crate tag for a
   version first shipped via Route A) → push tag → create GitHub
   release.

Tag scheme: `<crate>-v<version>` (e.g. `kevy-client-v1.5.0`).
Orthogonal to the workspace `vX.Y.Z` namespace, so the two routes
can coexist without colliding.

## Workflows in this directory

```
release.yml                   ← Route A trigger + body
release-crate.yml             ← Route B body (reusable, workflow_call)
release-kevy-bytes.yml        ┐
release-kevy-client.yml       │
release-kevy-config.yml       │
release-kevy-embedded.yml     │
release-kevy-hash.yml         │
release-kevy-madvise.yml      │
release-kevy-map.yml          │
release-kevy-persist.yml      │  Route B dispatch wrappers
release-kevy-resp-client.yml  │  (one per publishable crate;
release-kevy-resp.yml         │   ~20 LOC each, all the same shape)
release-kevy-ring.yml         │
release-kevy-rt.yml           │
release-kevy-store.yml        │
release-kevy-sys.yml          │
release-kevy-uring.yml        │
release-kevy.yml              ┘
```

Plus the everyday CI workflows (`ci.yml`, `docker.yml`, `fuzz.yml`)
which are unrelated to releasing.

## Picking between A and B

- A workspace bump where every crate moves → **Route A**.
- One crate ships, others stay still → **Route B** for that crate.
- Mixed (a few crates bump together) → Route A. Route B is one crate
  per dispatch; chaining 5 dispatches is correct but slower than one
  workspace tag.
- Want to backfill a `<crate>-v<version>` tag for a version that
  already landed via Route A → **Route B** with the same Cargo.toml
  version. The "already uploaded" path is idempotent: `cargo publish`
  skips, the per-crate tag still gets pushed.

## Secrets / permissions

- `CARGO_REGISTRY_TOKEN` (repo secret) — both routes use it for
  `cargo publish`. Set once at the repo level; Route B wrappers
  pull it via `secrets: inherit`.
- `GITHUB_TOKEN` — auto-injected by Actions in every job. Route B's
  reusable workflow uses it for tag push + `gh release create`
  (needs `permissions: contents: write`, already declared inside
  `release-crate.yml`).

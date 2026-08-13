---
name: release
description: Ship a kevy version — align every layer that carries a version number, tag, publish to crates.io/npm/GitHub, deploy the site, and verify each channel by content. Use when cutting any vX.Y.Z release, when a release channel looks stale or inconsistent, or when preparing the RC report that asks the owner to pull the trigger.
---

# Releasing kevy

A kevy release touches more surfaces than it looks like it does. Every
mistake catalogued here is one this project actually made, most of them
inside a single week. Read the layer list and the trap list before you
touch anything; run the gates rather than trusting your reading.

**The owner pulls the trigger.** Everything up to the tag is yours.
Pushing the tag publishes 40 crates to crates.io, which cannot be
undone, under the owner's company identity — so the tag push waits for
an explicit instruction, every time, no matter how green everything is.

## 1. What carries a version (six layers)

A bump that moves one layer and forgets another ships a lie. The
5.1.0 bump moved layer 1 and stopped; fourteen language doors kept
declaring 5.0.0, and two declared it in *bytes* — while 5.0.0 had a
compression defect that can make a stored value unreadable. Those doors
would have shipped the hazard the release existed to close.

| # | Layer | Where | Why it bites alone |
|---|---|---|---|
| 1 | Cargo | workspace `version`, every `path = "../kevy-x", version = "…"` pin — **including exact `=5.0.0` pins and hand-aligned whitespace** | a stale pin resolves a new crate against an old sibling |
| 2 | Language manifests | `package.json` ×6, `pyproject.toml`, `pubspec.yaml`, two `.csproj`, `build.gradle`, the server-binary npm package, the wasm package template | what the package manager stamps on the artifact |
| 3 | Inter-package pins | our own `@goliapkg/*` referring to each other | a new door installs old companions |
| 4 | Live constants | python `__version__`, gradle `version` / `versionName` | code, not metadata — it answers at runtime |
| 5 | Prose claims | `README` "this door tracks kevy X", `PUBLISH-FORM.md` | a reader trusts these more than the manifest |
| 6 | **Vendored engine bytes** | `bindings/*/android/**/jniLibs/*.so`, `bindings/*/ios/*.xcframework/**/*.a` | these do not *say* a version, they **are** one |

Run the gate, do not eyeball it:

```sh
python3 tools/check_version_alignment.py
```

It knows the independent tracks (`kevy-client` / `kevy-client-async`
keep their own 2.x line) and ignores example apps and third-party
lockfiles. It is in CI. Each layer also carries a floor, so a layer
that finds *nothing* fails instead of passing — a bare checkout with no
vendored artifacts built must not read as "everything agrees".

When it flags vendored bytes, rebuild:

```sh
# Android — both doors: kevy-ffi (dart:ffi / C++) and kevy-jni (Kotlin)
bash packaging/android/build-ffi-jnilibs.sh
bash packaging/android/build-jnilibs.sh
# Apple — the xcframework KevyKit wraps
bash packaging/apple/build-xcframework.sh bindings/apple/KevyKit/Artifacts
# Then re-vendor into each door that carries a copy
(cd bindings/nitro   && bash scripts/prepare-native.sh)
(cd bindings/expo    && bash scripts/prepare-native.sh)
(cd bindings/flutter && bash scripts/prepare-native.sh)
bash bench/vendorgate.sh
```

The engine's C ABI self-reports its version, so a byte-level answer is
one command away and beats any amount of reasoning:

```sh
strings <artifact> | grep -oE '^[0-9]+\.[0-9]+\.[0-9]+$' | sort -u
```

## 2. Before the tag

Every one of these has failed at least once here.

1. **Worktree clean, and no runtime residue in the repo root.**
   `bash bench/rootgate.sh` — a test that spawned a server in the
   source directory leaves `dump-*.rdb` / `feed-*.meta` behind.
2. **CI green on the exact commit you will tag.** The only posture is
   `gh run watch <id> --exit-status`; `gh run view -q .conclusion` is a
   query, not a gate — it exits 0 whatever it prints. Note that a
   *cancelled* run is not a failure: pushing again cancels the previous
   run by concurrency group.
3. **Version alignment** (§1) and **publish order**
   (`python3 tools/check_publish_order.py`).
4. **The self-hosted runner is online** — the release workflow's verify
   job runs on it. `gh api orgs/goliajp/actions/runners` (the runners
   are registered at the *org*, not the repo; the repo endpoint returns
   an empty list and that is not a diagnosis).
5. **Gates that own the release's claims**: perfgate, crashgate,
   repligate, availgate, tailgate. Run them on the box as `kevybench`,
   and give tailgate a real disk: `TMPDIR=$HOME/captmp`. On tmpfs its
   numbers are fiction and the firehose fills 32 GB of RAM.
6. **A release-profile build of the whole workspace** — `cargo build
   --release --workspace` must exit 0 before the tag, not during it.

## 3. Tag and publish

```sh
git tag -a vX.Y.Z -m "…" <commit>
git push origin vX.Y.Z          # ← this is the publish trigger
```

Then watch the whole workflow: `gh run watch <id> --exit-status`.

Three traps this project has paid for, all now guarded but worth
recognising:

- **The chain must be a topological order including dev-dependencies.**
  `cargo publish` resolves *every* version-gated dependency against
  crates.io, and a dev-dependency written with a version is resolved
  like any other — `kevy-cluster-rw`'s dev-dep on `kevy-rt` forced a
  retag. `tools/check_publish_order.py` now proves the order in CI.
- **The chain must cover every publishable crate.** A new crate that
  nobody added to the loop is silently skipped, and the tag has already
  published everything else by the time you notice.
- **npm's version comes from the tag, not from the committed
  `package.json`**, which drifts for a whole release line. The workflow
  derives it; do not "fix" the committed file to match.

**Go is not in that workflow, and cannot be.** Go has no registry: an
import path is a repository URL, so publishing means pushing a second
repository. It needs write access to `goliajp/kevy-go`, so it runs from
a machine that has it, right after the tag push:

```sh
bash scripts/mirror-go-module.sh --push X.Y.Z
```

That regenerates `kevy-go` from `bindings/go` (pure-Go files only — the
`kevy_embedded` half is cgo against a per-platform static library that
cannot travel through the module proxy), proves it builds and passes
its tests standing OUTSIDE the kevy tree, then pushes and tags it.
Never edit `kevy-go` directly; the next release overwrites it. A Go
version is immutable once the proxy has fetched it — moving a tag
changes nothing for anyone who already resolved it — so the script
refuses a version already tagged.

**Flutter is the same arrangement, for a different reason.** `dart pub
publish` includes only what git tracks, and flutter_kevy's 11 MB of
xcframework and jniLibs are gitignored — publishing from this tree
yields a 20 KB archive that resolves, analyses clean, and has no engine
in it. The binaries live in `goliajp/kevy-flutter` so they do not enter
this repository's history at 11 MB per release:

```sh
packaging/android/build-ffi-jnilibs.sh
cd bindings/flutter && bash scripts/prepare-native.sh && cd ../..
bash scripts/mirror-flutter-package.sh --push X.Y.Z
```

The generator refuses to push unless the dry-run's own file list
contains the xcframework and the jniLibs, and unless the `.so` reports
the version being shipped. pub.dev then needs the FIRST version pushed
by hand (`flutter pub publish` from a clone of kevy-flutter); after
that its Admin tab enables GitHub Actions publishing on tag pattern
`v{{version}}`.

**PyPI and NuGet need no credential at all.** Both do trusted publishing
— a short-lived GitHub OIDC token traded for upload rights — configured
once against (owner, repository, workflow filename). Run
`.github/workflows/pypi.yml` and `nuget.yml` from the Actions tab with
`publish: true`. Renaming either workflow file invalidates the policy,
and the next release then fails at the token exchange with an error
that says nothing about a rename. NuGet's policy is only *temporarily*
active for 7 days until a first publish locks it to the repository IDs,
so configure and publish in one sitting.

Both ship the MANAGED half only, for the reason the Go module does:
the embedded engine is a per-platform native library, and the first
embedded call raises a message naming where to get it.

## 4. Verify each channel — by content, never by status code

A soft 404 serves the home page with HTTP 200. A registry that has
never heard of your package answers 404 while the *engine* is fine. Ask
each channel what version it has, and read the answer.

```sh
# crates.io — every crate in the chain, not a sample
python3 - <<'EOF'
import json,re,urllib.request,pathlib
# `.replace("\\", " ")` first: the line-continuation backslashes are not
# crate names, and a snippet that silently carries junk is worse than none.
chain = re.search(r"for c in \\\n(.*?); do",
    pathlib.Path(".github/workflows/release.yml").read_text(),
    re.S).group(1).replace("\\", " ").split()
for c in chain:
    r = urllib.request.Request(f"https://crates.io/api/v1/crates/{c}",
                               headers={"User-Agent": "kevy-release-check"})
    print(c, json.load(urllib.request.urlopen(r))["crate"]["max_version"])
EOF

npm view @goliapkg/kevy version
gh release view vX.Y.Z --json tagName,isDraft,assets

# Go — ask the PROXY, not the repository. A user's `go get` resolves
# through proxy.golang.org, which fetches on first request and caches
# independently; a pushed tag the proxy has not taken is not published
# as far as anyone downstream is concerned. The per-version endpoint is
# the one that matters: `@latest` and `@v/list` can answer correctly
# while `.info` still 404s, because they are separately cached.
curl -sf https://proxy.golang.org/github.com/goliajp/kevy-go/v5/@v/vX.Y.Z.info

# ...and they are in SERIES. sum.golang.org hashes the zip it gets from
# the proxy, so while the proxy 404s the checksum database cannot
# record anything, and `go get` fails on the sumdb with the proxy's
# error wearing the sumdb's URL. Diagnose which layer is stuck by
# bypassing both — if this works, the artifact is fine and you are
# waiting on a cache:
#   GOPROXY=direct GOSUMDB=off go get github.com/goliajp/kevy-go/v5@vX.Y.Z

# Maven Central — repo1 is the authority. The Portal saying "PUBLISHED"
# is its own bookkeeping, and it says that before repo1 serves the file.
curl -sfI https://repo1.maven.org/maven2/jp/golia/kevy/X.Y.Z/kevy-X.Y.Z.pom
```

Then **install what you published** — both channels, because they fail
differently:

```sh
# crates.io: a fresh crate, compiled and run
cargo new /tmp/smoke && cd /tmp/smoke && cargo add kevy-resp@X.Y.Z && cargo run
# binary: checksum first, then start it and ask it its version
gh release download vX.Y.Z -p 'kevy-vX.Y.Z-<triple>.tar.gz*'
shasum -a 256 -c kevy-vX.Y.Z-<triple>.tar.gz.sha256
./kevy --port 6399 --dir "$(mktemp -d)" &
kevy-cli -p 6399 INFO server | grep kevy_version
```

Two crates are *supposed* to disagree: `kevy-client` and
`kevy-client-async` ride their own 2.x line. Everything else must match
the tag.

## 5. After the tag

- **Fast-forward `master`.** `GIT-FLOW.md` says every `vX.Y.Z` tag
  points at master, and for two whole release lines that was false —
  master sat 1216 commits behind at 3.8.0, so a downstream user
  fetching `raw.githubusercontent.com/.../master/CHANGELOG.md` got
  release notes that stopped two majors ago and reported the changelog
  as "empty". They were reading exactly what we served.
  `git push origin develop:master`.
- **Rebuild the site's wasm before rsync.** The docs will say the new
  version while the playground still runs the old engine otherwise:

  ```sh
  cargo build -p kevy-wasm --target wasm32-unknown-unknown --release
  cp target/wasm32-unknown-unknown/release/kevy_wasm.wasm crates/kevy-wasm/pkg/kevy.wasm
  cp crates/kevy-wasm/pkg/{kevy.js,kevy.d.ts,kevy-opfs-worker.js,kevy.wasm} site/demo/pkg/
  python3 -m http.server 8901 --directory site &   # the verifier needs this
  node tools/verify_play.mjs                       # drives real Chrome, 13 assertions
  python3 tools/gen_docs_site.py && python3 tools/gen_docs_site.py --check
  rsync -av --delete site/ t01:/apps/kevy/web/
  ```

  Then confirm the deployed wasm is the one you built:
  `curl -s https://kevy.golia.jp/demo/pkg/kevy.wasm | shasum -a 256`
  against the local file.
- **Check the release notes are reachable without branch archaeology**:
  <https://kevy.golia.jp/changelog/> must be a real page (grep the
  `<title>`, not the status code) and must contain the new version.
- **Tell downstream**, and *tailor it*: a notice written as if everyone
  uses every feature asks people to verify things they structurally do
  not have. Check what they actually enable first. Say plainly what
  behaviour they will observe change.

## 6. The failure catalogue

Recognise these; each one cost a debugging round.

- **A measurement device's failure looks exactly like data.** A gate
  whose probe died printed empty medians, and the `${x:-999999999}`
  default rendered them as four bars over the limit. A high-water
  `fetch_max` gauge got read as a rate. A `grep -c` returned a file's
  line count and was read as a hit count. **Suspect the apparatus
  before the conclusion, every time a number surprises you.**
- **A criterion an empty input satisfies is not a criterion.** Ask:
  *would an empty data directory give the same answer?* `recovered >=
  synced` is vacuous when `synced` is 0. A "must be absent" assertion
  is trivially true on an empty store and must be paired with a "must
  be present" one. (See `.claude/rule/hygiene.md`.)
- **Merging two green branches can produce a red one.** Site pages that
  were regenerated on one branch go stale when the other branch's
  markdown lands. Re-run the doc gates *after* the merge.
- **Dates in code comments trip `commentgate`.** Cite a finding by
  subject, not by `FINDING-YYYY-MM-DD-…` filename.
- **Machine load fakes failures.** Harness spawn timeouts go red when
  something else is compiling; they go green on a quiet machine. A/B it
  before filing it.
- **A new page can expose an old defect.** Publishing the changelog
  turned up 29 dead links of its own *and* 26 pages of `blob/main`
  links to a branch this repo does not have. The gate was always right;
  the page was new.
- **A pipeline exit status is the last command's, not the interesting
  one's.** `go build … 2>&1 | tail -5` reports `tail`'s success, so a
  `&&` after it fires on a failed build. This ate a verdict here for at
  least the third time. Redirect to a file and check `$?`, or drop the
  pipe.
- **A publish has three separate audiences, and reaching one is not
  reaching the others.** Go makes this explicit: pushing the tag, the
  module proxy fetching it, and `sum.golang.org` recording its checksum
  are three events minutes apart, and `go get` fails with `unknown
  revision` while the middle one has already succeeded. The same shape
  is why npm 404s a package it just accepted and why the Maven Portal
  says PUBLISHED before repo1 serves the file. Verify at the layer the
  *user* resolves through, and expect to wait.
- **Reading an artifact is not installing it.** `kevy` 5.1.0 went to
  NuGet declaring a dependency on a package that cannot exist
  (`IsPackable=false` on the project it referenced), so every restore
  failed with NU1101. The nuspec was inspected before the push and the
  dependency line was read as "publish both" — a coherent reading, and
  wrong, and one `dotnet restore` would have settled it. Nothing in the
  repository could catch it: every test builds from the projects, not
  the package. **Every publish workflow must install its own output
  from a local feed and run it against something real before it
  pushes.** The Python one did; the .NET one did not; that is the whole
  difference between the two outcomes.

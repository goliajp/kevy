# Every gate read the tree; none asked the world

**Date** 2026-08-30 · **Found while** verifying that v6.0.0 was released ·
**Status** closed — seven doors published, two gates added

## What was true

v6.0.0 was tagged, published, and reported verified across "every channel".
Nine channels had been checked by content, individually, including all 41
crates on crates.io. Every gate the repository owns was green:
`check_version_alignment` found 219 consistent declarations across seven
layers, `check_door_changelogs` was satisfied, CI was green, and the vendored
engine bytes in every binding self-reported 6.0.0 under `strings`.

A tenth query, made only because the verification loop was still running,
came back:

```
Maven jp.golia:kevy    5.3.0
```

Pulling that thread found six more:

| door | package | tree | registry |
|---|---|---|---|
| node | `@goliapkg/kevy-node` | 6.0.0 | **5.1.0** |
| ts | `@goliapkg/kevy-ts` | 6.0.0 | **5.1.0** |
| electron | `@goliapkg/kevy-electron` | 6.0.0 | **5.1.0** |
| expo | `expo-kevy` | 6.0.0 | **5.1.0** |
| nitro | `react-native-kevy-nitro` | 6.0.0 | **5.1.0** |
| java | `jp.golia:kevy` | 6.0.0 | **5.3.0** |
| tauri | `tauri-plugin-kevy-api` | 6.0.0 | **404 — never published** |

The last one is documented in `bindings/tauri/README.md` as
`npm install tauri-plugin-kevy-api`. It was an install command that could
not work, for as long as the file has existed.

Asking the same question about earlier releases showed this was not new:

```
PyPI kevy          5.1.0, 5.2.0, 5.3.0, 6.0.0     ← no 5.4.0, no 5.4.1
NuGet kevy         5.1.0, 5.2.0, 5.3.0, 6.0.0     ← no 5.4.0, no 5.4.1
pub.dev            5.1.0, 6.0.0                   ← no 5.2, 5.3, 5.4.x
Maven              5.1.0, 5.3.0, 6.0.0            ← no 5.0, 5.2, 5.4.x
```

v5.4.0 and v5.4.1 reached four channels of thirteen.

## Why every gate passed

Nothing was broken and nobody skipped a step, because for five of these
doors **there was no step**. The tag workflow's npm job publishes one
package — the wasm build — and names it literally. The five binding
packages had been published once, by hand, at 5.1.0, and nothing ever
published them again. Maven is `workflow_dispatch` only, deliberately
(Central cannot be withdrawn), so it opens only when a human presses it;
twice, nobody did.

The gates could not see any of this. Every one of them reads the *tree*:
do our files agree with each other, do the vendored bytes match the
manifests, does the compat claim match the verb list. The tree was
internally consistent and telling the truth about itself. The question
nobody had a gate for was **does the world have what we said we shipped**.

This is the same shape as every instrument defect this release found — an
instrument that answers a question next to the one being asked — except
here the instrument was the whole gate suite, and the adjacent question was
a good one that it answered correctly.

## What changed

**Published**, after the audit: the five npm binding packages at 6.0.0
(byte-identical to the tag; `git diff v6.0.0 HEAD -- bindings/` was one
CHANGELOG file), `tauri-plugin-kevy-api` at 6.0.0 (first publish, so the
README's install line now works), `jp.golia:kevy` 6.0.0 to Maven Central
(dispatched from the tag, which the script requires), and `flutter_kevy`
6.0.0 to pub.dev (first publish, which is manual per package).

**`tools/check_channels_published.py`** — asks nine registry kinds whether
they serve the newest release tag. The door list is derived from the tree,
never written down: a hand-kept list is how Maven was missed twice, and it
would have the same hole the day a fourteenth door is added. 54 doors
resolved on the first green run.

Two things it had to learn the hard way, both caught by running it:

- Its first version collapsed "404, never published" and "the network
  refused me" into one `None`, and reported that it could not tell when
  what it had actually learned was that the package was absent.
- `kevy-client` and `kevy-client-async` are deliberately on a 2.x line.
  Asking them for 6.0.0 finds nothing, which reads like a finding and is
  not one. Each door is now asked about the version it declares.

The GitHub release is a door too, and it is checked by content: not
draft, and carrying at least one asset. `release.yml` builds the server
binaries in a job separate from the publishing ones, so a tag can end up
with every registry agreeing and a Releases page with nothing on it.

It also refuses on a `bindings/` directory it cannot read — a door added
in a manifest format nobody taught it would otherwise be skipped in
silence, which is this same finding one level up. Verified by putting a
`bindings/ruby/kevy.gemspec` in the tree: exit 2, naming the directory.

Red-green: green on 6.0.0 (54/54), red on 5.4.1 (12 doors, exit 1), and
three of those twelve were confirmed by hand against the registries before
being believed.

**`release.yml`** now publishes every binding package, found with `find`
rather than listed, with a floor that fails if the search returns fewer
than six — a broken search publishes nothing while reporting success.

Running that step outside a release — by lifting the shell out of the
YAML and stubbing the two mutating commands — was worth more than writing
it carefully. It showed the already-published skip working (6 publishable,
0 published, at v6.0.0) and the publish path working (6 at a version that
does not exist). It also showed two things wrong with what had been
written:

- the floor counted *files the find turned up*, not packages the step took
  responsibility for, so a tree where every manifest had gone private
  would have passed while publishing nothing;
- with the `-not -path '*/node_modules/*'` removed, the loop walks into
  `node_modules` and offers to publish third-party packages under our
  version. It reached for `undici-types`, which is on a 6.x line of its
  own and so passed every check. The find flag is now backed by an
  independent `case */node_modules/*` in the loop body, because one filter
  is a single point of failure for something that publishes.

**`ci.yml`** now builds and typechecks `bindings/tauri/guest-js`, which
was neither built nor tested here. Its `main` points at `dist/index.js`,
so the step asserts that file exists rather than trusting `tsc`'s exit
code.

**`check_version_alignment.py`** learned CMake. `bindings/cpp/CMakeLists.txt`
said `VERSION 5.0.0` — three releases behind, unnoticed, because nothing
consumes it and no layer scanned it. The gate's own comment already said
why: *"the gate could only see the formats it had been taught."*

**`channel-parity.yml`** runs the new gate daily. It asks about the newest
tag rather than the tree, so it is green between releases and red only when
a release was announced that a user cannot install.

## The one defect publishing found

Installing the seven packages from the registry — rather than trusting the
publish receipts — turned up a real one. `tauri-plugin-kevy-api@6.0.0`
declares `"type": "module"` and compiles under `moduleResolution:
"Bundler"`, which permits extensionless relative imports. `tsc` is happy;
so is Vite, which is what a Tauri app uses. Node's ESM resolver is not:

```
import { kevy, replyInt } from 'tauri-plugin-kevy-api'
→ ERR_MODULE_NOT_FOUND  .../dist/reply
```

That is the exact line the README teaches. No user is broken today — the
package's only documented environment is a Tauri app behind a bundler —
but the package claims to be ESM and is not, outside one.

Fixed in the tree (`./reply` → `./reply.js` in five specifiers, which both
resolutions accept) and asserted in CI, which now imports the built module
under plain Node and checks three exports are present. Verified red-green
by reverting the specifiers: `ERR_MODULE_NOT_FOUND`, then green again.

**npm 6.0.0 keeps the defect.** A version cannot be republished, and
unpublishing the only version of a package blocks the name for 24 hours.
The fix therefore rides the next release. Cutting a 6.0.1 for one door
would put the tree at two versions at once, which is the invariant the
rest of this finding is about — that call belongs to the owner.

## The eighth door, found by the gate itself

With the seven registries green, `kevy.golia.jp` was still serving
`{"version":"5.4.1","rev":"25f02fcb"}`. The site is a release door — it
ships an engine and announces a version, and the release skill has said
"rebuild and re-verify the site, then deploy it" all along. It was skipped
the same way Maven was.

Adding it to the gate turned the gate red on a door nobody had named:
55 of 56. That is the first time it found something rather than confirming
something already known.

Nothing about that host can be checked by status code. Every path answers
200 with the SPA index — a filename invented on the spot came back 200
with the same 2396 bytes as the home page. The check is that
`/build.json` parses as JSON and names the version, which the index
cannot do.

Rebuilding to deploy turned up a second thing. The locally built wasm
carried **25 copies of `/Users/<name>/.rustup/...`**; the published one
carries the `/rustc/<hash>/` form. Same command, no remap configured
anywhere — the difference is that this machine has the `rust-src`
component, so rustc resolves std's panic locations to the local toolchain
source. CI has no rust-src and so never showed it, and the two artifacts
differed by 4 KB for a reason nobody had looked at. Deploying it would
have put the builder's home directory on a public website.
`web/engine.mjs` now passes `--remap-path-prefix=$HOME=~`; the rebuilt
artifact has zero occurrences of the name.

Deployed and verified against the live host rather than the local build:
`check.mjs` 768 pages, 17580 links, all at 6.0.0, byte-identical across
two builds; `verify.mjs https://kevy.golia.jp` 28/28 in real Chromium;
`/changelog/` a real page whose `<title>` is "Release notes · kevy" and
which contains 6.0.0. The gate: **all 56 doors serve 6.0.0**.

## What is still deliberately unpublished

`bindings/tauri/tauri-plugin-kevy` — its README installs it by path and
says "or, once published". Publishing needs versions on its `kevy-*` path
dependencies first, which is a decision, not an oversight. The stale `= "4"`
in that line is now `= "6"`. Recorded in the gate's `NOT_PUBLISHED` map with
that reason, alongside `bindings/android` (documentation), `bindings/apple`
(SwiftPM resolves the tag) and `bindings/cpp` (CMake FetchContent).

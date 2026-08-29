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

Red-green: green on 6.0.0 (54/54), red on 5.4.1 (12 doors, exit 1), and
three of those twelve were confirmed by hand against the registries before
being believed.

**`release.yml`** now publishes every binding package, found with `find`
rather than listed, with a floor that fails if the search returns fewer
than six — a broken search publishes nothing while reporting success.

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

## What is still deliberately unpublished

`bindings/tauri/tauri-plugin-kevy` — its README installs it by path and
says "or, once published". Publishing needs versions on its `kevy-*` path
dependencies first, which is a decision, not an oversight. The stale `= "4"`
in that line is now `= "6"`. Recorded in the gate's `NOT_PUBLISHED` map with
that reason, alongside `bindings/android` (documentation), `bindings/apple`
(SwiftPM resolves the tag) and `bindings/cpp` (CMake FetchContent).

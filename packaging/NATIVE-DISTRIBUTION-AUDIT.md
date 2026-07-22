# Native-library distribution: which packages carry their engine, and which don't

Every kevy language package embeds the same Rust engine as a native
library (cdylib / static lib / xcframework). The question this audit
answers is the one a user hits on `install`: does the package **carry**
that library so the runtime finds it on its own, or does it only work from
a checkout with an env var or a relative path pointing back into the repo?

Only npm carries it. The other four are checkout-shaped, and that is a t6
publish-form task, not four unrelated bugs — it is one pattern, recorded
here once.

## The scoreboard

| Package | Load mechanism | Carries the native? | Gap |
|---|---|---|---|
| **npm** (`@goliapkg/kevy-node`) | platform package via `optionalDependencies`, loader resolves into it | **yes** | none — `npm-install-smoke` proves it green on both CI legs |
| **NuGet** (`kevy`) | .NET RID graph → `runtimes/<rid>/native` | **no** | packs the wrong project; the shippable `kevy` reaches the engine through a ProjectReference that does not travel in the package. See `nuget/PACK-GAP.md` |
| **Go** (`kevy-go`) | cgo `#cgo LDFLAGS` | **no** | links `${SRCDIR}/../../target/debug/…`, a path that leaves the module. See `../bindings/go/PUBLISH-FORM.md` |
| **SwiftPM** (`KevyKit`) | `binaryTarget` | **no** | `path: "Artifacts/Kevy.xcframework"` is a local path; a tag-published package needs a remote `url:` + checksum, or the xcframework committed |
| **Maven** (`jp.golia:kevy`) | `System.loadLibrary("kevy_jni")` | **no** | the `.so` is only on `java.library.path` at test time (cargo target dir); the jar bundles nothing, so an installed jar cannot find libkevy_jni. (Android's AAR is separate — this is the desktop-JVM jar) |

## The shared shape of the fix

Each non-npm package needs to do what npm already does: put the native
artifact **inside** the published unit, per target, and load it from there
rather than from the repo.

- **NuGet** — a pack target that bundles the `Kevy.Embedded` assembly plus
  `runtimes/<rid>/native/libkevy_ffi.{so,dylib}` for each shipped RID.
- **Go** — vendor `include/` and `libs/<goos>_<goarch>/libkevy_ffi.a`,
  switch the cgo preamble to `${SRCDIR}`-relative (already specced in
  PUBLISH-FORM.md), and resolve the `/vN` module-path question.
- **SwiftPM** — a remote `binaryTarget(url:checksum:)` against the
  xcframework attached to the release tag.
- **Maven** — bundle the per-OS/arch `libkevy_jni` into the jar under a
  resource path and extract-and-load at startup (the standard JNI-in-a-jar
  pattern), or publish a classifier artifact per platform.

## Why this is t6, not now

All four are publish-form: they only matter once there is a tag and a
registry to publish to, and each carries a real decision (Go's `/vN`,
Maven's bundle-vs-classifier, SwiftPM's checksum pinning). The engineering
that CAN be done without publishing — the packaging scripts and their
install smokes — is done for npm and is the template. The rest is the
channel-release owner's, tracked here as one task with four legs instead of
four scattered surprises.

## What each proves *today*

The conformance suites are all green, but they load the engine from the
checkout (`KEVY_FFI_LIB`, `java.library.path`, cgo into `target/`), which
tests the **binding**, not the **package**. npm is the only one whose smoke
also tests the package. That distinction is the whole point of this audit:
a green binding is not a shippable package.

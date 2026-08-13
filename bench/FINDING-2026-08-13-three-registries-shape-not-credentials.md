# Three registries, and none of them was blocked on a credential

PyPI, NuGet and pub.dev sat on the roadmap for weeks as "missing
credentials — not something I can supply". All three were wrong about
what was blocking them, in the same direction: the credential was a
two-minute click, and the package was not publishable.

Recorded next to the Go finding because it is the same shape a fourth,
fifth and sixth time: **a check that passed for a reason other than the
one intended.** There the module built inside the tree it was extracted
from. Here, three packages passed every test this repository runs and
would each have shipped something that does not work on a user's
machine.

## What was actually true

| | credential | real blocker |
|---|---|---|
| PyPI | none needed — OIDC trusted publishing, and *pending publishers* cover a project that does not exist yet | wheel carried no engine, loader pointed outside the wheel |
| NuGet | none needed — OIDC trusted publishing | first embedded call threw an unexplained `DllNotFoundException` |
| pub.dev | none needed — OIDC, but the *first* version must be published by hand | `pub publish` produced a 20 KB archive with no engine in it |

## Python: the loader pointed at a tree the wheel is not in

`pip install` into a clean venv, then `mem://`:

    io error: libkevy_ffi not found ...
    Tried: /private/tmp/pyv/lib/target/release/libkevy_ffi.dylib

`_candidate_paths` walks three directories up from the package and looks
for `target/{release,debug}` — a layout that exists only inside a kevy
checkout. Every test in the repository passes because every test runs
inside one.

The answer is the Go module's answer, because it is the Go module's
problem: publish the pure half. Twenty of the twenty-two modules are
stdlib-only RESP client; ctypes appears in one. `mem://` and `file://`
now name where to get the engine and say that a `kevy://` URL needs
none of it.

**The trap inside the verification.** Testing this from
`bindings/python` proves nothing: with the source tree on `sys.path`,
`import kevy` picks IT up rather than the wheel, and its relative search
finds the engine three directories up. The first run of this check went
green that way and reported the engine as present. It has to run from a
neutral directory, and the workflow asserts `site-packages` is in
`kevy.__file__` so a future run cannot quietly drift back.

That is the second time this exact import has fooled a check here.

## .NET: an exception that named nothing

P/Invoke resolves lazily, so the managed client works and the failure
waits for the first embedded call, where the runtime says:

    Unable to load shared library 'kevy_ffi' or one of its dependencies

A reader cannot tell from that whether they typed something wrong, are
on an unsupported platform, or skipped a step. `CheckAbi` is the single
boundary every embedded entry point passes through, so it converts the
failure into the sentence the other two doors give, keeping the original
as `InnerException`. Verified both ways — the message appears without a
library, and the engine still loads with `KEVY_FFI_LIB` set. A fix that
only shows up in the failing direction is half-checked.

## Flutter: a 20 KB package that resolved and analysed clean

`dart pub publish` includes only what git tracks. The 11 MB of
xcframework and jniLibs are built by `prepare-native.sh` and excluded by
`.gitignore`, so the archive contained the Dart and nothing else. It
would have installed, resolved, analysed, and failed at
`DynamicLibrary.open`.

Tracking them in kevy means 11 MB per release in this repository's
history forever, for a payload only one door needs. So they go in
`goliajp/kevy-flutter`, generated — the kevy-go arrangement, for the
same reason: the published thing has requirements the source tree
should not carry. pub.dev does not verify that `repository:` matches
where a package was published from (checked — there is no provenance
badge to lose), so the pubspec still points at `goliajp/kevy`.

**The generator's check caught its own first version.** The staging step
copied the source `.gitignore` forward, carrying the exclusion into the
one place it is exactly wrong, and produced a 16 KB archive that looked
entirely healthy. The size and content check on the dry-run's own file
list is what turned that from a shipped defect into a two-minute fix.

## What vendorgate does and does not say

vendorgate verified all four Flutter artifacts as current, every run,
while the publishable form of them was empty. That is not a bug in it:
its header says absent gitignored artifacts are skipped, and "is the
binary on disk current for this ABI" is a real question. It is simply a
different question from "does the binary reach a user", and the two look
alike enough to be mistaken for each other.

The header says so now, and points at what answers the second one. Same
discipline as pushgate declaring the CI steps it does not cover: a
partial check has to name its own boundary, or it reads as a total one.

## The generalisation

Every one of these was found by asking what an installed package does,
rather than what the repository's tests do. The tests were all green and
all of them ran inside the tree.

For anything that gets published, the check has to stand where the user
stands: outside the tree, from a fresh install, with the thing the build
was supposed to produce deleted first.

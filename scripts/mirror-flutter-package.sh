#!/usr/bin/env bash
# Generate github.com/goliajp/kevy-flutter from bindings/flutter, with
# the native engine in it, and prove the engine is actually in there.
#
#   bash scripts/mirror-flutter-package.sh              # generate + verify
#   bash scripts/mirror-flutter-package.sh --push 5.1.0 # ... then push + tag
#
# WHY A SEPARATE REPOSITORY
#
# `dart pub publish` includes only files git tracks. flutter_kevy's
# engine — a 6.2 MB xcframework and 4.6 MB of jniLibs — is built by
# bindings/flutter/scripts/prepare-native.sh and deliberately not
# tracked, so publishing straight from this tree produces a 20 KB
# archive with no engine in it. That is not hypothetical: it is what
# `flutter pub publish --dry-run` produced here.
#
# Tracking 11 MB per release inside kevy would put it in this
# repository's history forever, for every clone, for a payload only the
# Flutter door needs. So the binaries live in a generated artifact
# repository instead — the same arrangement as kevy-go, for the same
# reason: the published thing has requirements the source tree should
# not have to carry.
#
# pub.dev does not verify that a package's `repository:` matches where
# it was published from (checked: there is no provenance badge to lose),
# so the pubspec keeps pointing at goliajp/kevy, where the source and
# the issues are.
#
# The engine is built by bindings/flutter/scripts/prepare-native.sh,
# which needs an Apple toolchain and the Android NDK cross-targets. This
# script does not paper over that: it fails and says what is missing,
# because a mirror built without an engine is exactly the failure this
# exists to prevent.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/bindings/flutter"
# Overridable so the push path can be exercised against a local bare
# repository; a release uses the default.
REPO="${KEVY_FLUTTER_REPO:-git@github.com:goliajp/kevy-flutter.git}"

# --dry-run VERSION does everything --push does and ends in
# `git push --dry-run`. Re-running --push for a version whose tag already
# holds this content says so and pushes nothing.
PUSH_VERSION=""
DRY_RUN=0
case "${1:-}" in
    --push|--dry-run)
        [ "$1" = "--dry-run" ] && DRY_RUN=1
        PUSH_VERSION="${2:-}"
        [ -n "$PUSH_VERSION" ] || { echo "✗ $1 needs a version, e.g. $1 5.1.0" >&2; exit 2; }
        ;;
esac

STAGE="$(mktemp -d "${TMPDIR:-/tmp}/kevy-flutter-mirror.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT
OUT="$STAGE/kevy-flutter"

echo "→ the engine, from its authoritative build"
for a in "$SRC/ios/kevy_ffi.xcframework" \
         "$SRC/android/src/main/jniLibs/arm64-v8a/libkevy_ffi.so" \
         "$SRC/android/src/main/jniLibs/x86_64/libkevy_ffi.so"; do
    [ -e "$a" ] || {
        echo "✗ missing $a" >&2
        echo "  Build it first, from the repo root:" >&2
        echo "    packaging/android/build-ffi-jnilibs.sh" >&2
        echo "    cd bindings/flutter && bash scripts/prepare-native.sh" >&2
        echo "  Publishing without it is the 20 KB archive this script exists" >&2
        echo "  to make impossible." >&2
        exit 1; }
done

# The engine must be the version being shipped. It self-reports over its
# C ABI, so `strings` settles it — the same check the version-alignment
# gate uses on the other vendored artifacts, applied here because this is
# the copy that reaches users.
if [ -n "$PUSH_VERSION" ]; then
    for so in "$SRC"/android/src/main/jniLibs/*/libkevy_ffi.so; do
        got=$(strings "$so" | grep -oE '^[0-9]+\.[0-9]+\.[0-9]+$' | head -1 || true)
        [ "$got" = "$PUSH_VERSION" ] || {
            echo "✗ $so reports $got, publishing $PUSH_VERSION" >&2
            echo "  Rebuild the natives before publishing; a stale engine in a" >&2
            echo "  new version is a lie told in bytes." >&2
            exit 1; }
    done
    echo "  engines self-report $PUSH_VERSION"
fi

echo "→ staging"
# Everything git tracks under bindings/flutter, plus the untracked
# engine. `git ls-files` rather than cp -r: it is exactly the set that
# would have been published from here, so what changes is the engine and
# nothing else.
mkdir -p "$OUT"
(cd "$SRC" && git ls-files) | while read -r f; do
    mkdir -p "$OUT/$(dirname "$f")"
    cp "$SRC/$f" "$OUT/$f"
done
cp -R "$SRC/ios/kevy_ffi.xcframework" "$OUT/ios/"
mkdir -p "$OUT/android/src/main/jniLibs"
cp -R "$SRC/android/src/main/jniLibs/." "$OUT/android/src/main/jniLibs/"

# The example app is a demo of the package, not part of it, and it drags
# a pubspec that resolves flutter_kevy by relative path — which cannot
# resolve once this is a standalone repository.
rm -rf "$OUT/example"

# The workflow that turns the pushed tag into a pub.dev publish lives in
# bindings/flutter/.github and arrives with the tracked files above; there
# it is inert, at the mirror's root it is the repository's workflow.
[ -f "$OUT/.github/workflows/publish.yml" ] || {
    echo "✗ the mirror has no .github/workflows/publish.yml: the tag it pushes" >&2
    echo "  would publish nothing, exactly as through 6.3.0" >&2
    exit 1; }

# The source tree's .gitignore is what makes the engine untracked HERE,
# and copying it forward carries that decision into the one place where
# it is exactly wrong: over there the binaries are the payload. pub's
# file selection honours .gitignore, so leaving these lines in produced a
# 16 KB archive that resolved and analysed perfectly and contained no
# engine. Drop the two rules; keep the rest.
if [ -f "$OUT/.gitignore" ]; then
    grep -vE '^(ios/kevy_ffi\.xcframework/|android/src/main/jniLibs/)$' \
        "$SRC/.gitignore" > "$OUT/.gitignore"
fi

{
    printf '<!-- Generated by scripts/mirror-flutter-package.sh in goliajp/kevy. -->\n'
    printf '<!-- Do not edit here: edit bindings/flutter there. -->\n\n'
    printf '# kevy-flutter\n\n'
    printf 'Publishing artifact for [`flutter_kevy`](https://pub.dev/packages/flutter_kevy)\n'
    printf '— kevy embedded in a Flutter app over `dart:ffi`.\n\n'
    printf 'This repository is **generated** from `bindings/flutter` in\n'
    printf '[goliajp/kevy](https://github.com/goliajp/kevy) and exists so the\n'
    printf 'published package can carry the native engine (11 MB per release)\n'
    printf 'without that payload living in the engine repository forever.\n\n'
    printf 'Issues and pull requests belong on\n'
    printf '[goliajp/kevy](https://github.com/goliajp/kevy); anything committed\n'
    printf 'here is overwritten by the next release.\n\n'
    printf '```\nflutter pub add flutter_kevy\n```\n\n'
    printf 'Documentation: https://kevy.golia.jp\n'
} > "$OUT/README.md"

echo "→ verifying the package has an engine in it"
cd "$OUT"
# --dry-run reports what WOULD be uploaded. Reading its file list is the
# check: a Flutter package that resolves and analyses cleanly and ships
# no engine looks entirely healthy right up until DynamicLibrary.open.
if ! flutter pub publish --dry-run > "$STAGE/dry.txt" 2>&1; then
    # A warning exits non-zero too, so decide by reading rather than by
    # status: an unmentioned CHANGELOG version is not a reason to stop.
    if ! grep -q "Package has 0 warnings" "$STAGE/dry.txt"; then
        echo "  dry-run reported:"
        grep -E '^\s*\*|warning|error' "$STAGE/dry.txt" | head -10 | sed 's/^/    /'
    fi
fi
size=$(grep -oE 'Total compressed archive size: [0-9.]+ [KMG]B' "$STAGE/dry.txt" | tail -1)
echo "  $size"
for want in kevy_ffi.xcframework jniLibs; do
    grep -q "$want" "$STAGE/dry.txt" || {
        echo "✗ the archive does not contain $want." >&2
        echo "  This is the 20 KB failure: pub publish takes only what git" >&2
        echo "  tracks, and an engine that is present on disk but untracked is" >&2
        echo "  invisible to it. Check that the staging step committed it." >&2
        exit 1; }
done
echo "  archive contains the xcframework and the jniLibs"

# The dry-run says "The server may enforce additional checks" and means
# it: pub.dev rejected a package this reported as having 0 warnings,
# because LICENSE still held `flutter create`'s placeholder. Anything
# learned from a server refusal goes here, since the dry-run will not
# learn it.
[ -s "$OUT/LICENSE" ] || { echo "✗ no LICENSE in the package" >&2; exit 1; }
if grep -qi "TODO" "$OUT/LICENSE"; then
    echo "✗ LICENSE still contains a TODO — pub.dev refuses that outright," >&2
    echo "  and the dry-run does not: it reported 0 warnings on the file" >&2
    echo "  that got the package rejected." >&2
    exit 1
fi
grep -qiE "MIT|Apache" "$OUT/LICENSE" || {
    echo "✗ LICENSE names neither licence this project ships under" >&2; exit 1; }
echo "  LICENSE is a real licence"
cd "$ROOT"

if [ -z "$PUSH_VERSION" ]; then
    echo "→ comparing against the published mirror"
    CLONE="$STAGE/published"
    if git clone --quiet --depth 1 "$REPO" "$CLONE" 2>/dev/null; then
        # The dry-run above resolved dependencies and left .dart_tool and
        # a lockfile behind. They are build state, not package content,
        # and comparing them would report drift on every single run.
        rm -rf "$OUT/.dart_tool"
        if diff -r -x '.git' -x '.dart_tool' "$CLONE" "$OUT" > "$STAGE/drift.txt" 2>&1; then
            echo "  ✓ in sync"
        else
            echo "  ! kevy-flutter is behind bindings/flutter:"
            sed 's/^/    /' "$STAGE/drift.txt" | head -20
            echo "    Expected between releases — the release pushes it."
        fi
    else
        echo "  (cannot reach $REPO, or it is empty; generation was verified)"
    fi
    exit 0
fi

if [ "$DRY_RUN" = 1 ]; then
    echo "→ rehearsing the push of kevy-flutter v$PUSH_VERSION (nothing is pushed)"
else
    echo "→ pushing kevy-flutter v$PUSH_VERSION"
fi
WORK="$STAGE/push"
git clone --quiet "$REPO" "$WORK" 2>/dev/null || git init --quiet "$WORK"
git -C "$WORK" remote add origin "$REPO" 2>/dev/null || true

# What would be committed, which is what pub.dev would publish: the
# generated tree filtered by its own .gitignore. The dry-run's .dart_tool
# and lockfile fall out here as build state.
find "$WORK" -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} +
cp -R "$OUT"/. "$WORK"/
git -C "$WORK" add -A

# Asked of pub.dev, not of the tag: a version live there is fixed forever.
live=$(curl -sf "https://pub.dev/api/packages/flutter_kevy" 2>/dev/null \
    | grep -o "\"version\":\"$PUSH_VERSION\"" || true)

MOVE_TAG=0
if git -C "$WORK" rev-parse -q --verify "refs/tags/v$PUSH_VERSION" >/dev/null; then
    # A re-run. The tag either holds this package or it does not; the
    # engine is rebuilt every run and its bytes are not reproducible, so
    # the comparison takes the engine by its path and self-reported
    # version and everything else byte for byte.
    mkdir "$STAGE/tagged" "$STAGE/staged"
    git -C "$WORK" archive "refs/tags/v$PUSH_VERSION" | tar -x -C "$STAGE/tagged"
    git -C "$WORK" checkout-index -a --prefix="$STAGE/staged/"
    if python3 "$ROOT/scripts/compare-prebuilt-tree.py" "$STAGE/tagged" "$STAGE/staged" \
        "$PUSH_VERSION" ios/kevy_ffi.xcframework android/src/main/jniLibs; then
        echo "  ✓ already pushed v$PUSH_VERSION, content matches — nothing to push"
        if [ -n "$live" ]; then
            echo "  ✓ flutter_kevy $PUSH_VERSION is on pub.dev"
        else
            echo "  ! flutter_kevy $PUSH_VERSION is not on pub.dev yet (see below)"
        fi
        PUSHED=0
    elif [ -n "$live" ]; then
        echo "✗ flutter_kevy $PUSH_VERSION is already on pub.dev, and its tag on" >&2
        echo "  kevy-flutter holds different content. A published version is" >&2
        echo "  permanent there. Ship the next one." >&2
        exit 1
    else
        # Tagged but never published: a staging pointer nobody consumed,
        # so it may move.
        echo "  v$PUSH_VERSION is tagged but not published, and differs — moving the tag"
        MOVE_TAG=1
    fi
elif [ -n "$live" ]; then
    echo "✗ flutter_kevy $PUSH_VERSION is on pub.dev but kevy-flutter has no" >&2
    echo "  v$PUSH_VERSION tag, so what was published cannot be compared here." >&2
    exit 1
fi

if [ "${PUSHED:-1}" = 1 ]; then
    if git -C "$WORK" diff --cached --quiet; then
        echo "  content already current; tagging only"
    else
        git -C "$WORK" commit --quiet -m "flutter_kevy $PUSH_VERSION

Generated from goliajp/kevy bindings/flutter by
scripts/mirror-flutter-package.sh. Do not edit here."
    fi
    git -C "$WORK" tag -f "v$PUSH_VERSION" >/dev/null
    # One atomic ref update: two pushes leave a window in which the commit
    # exists without its tag, and whatever fetches during it caches that.
    # Only the tag may be forced, and only when it is being moved.
    tagref="refs/tags/v$PUSH_VERSION:refs/tags/v$PUSH_VERSION"
    [ "$MOVE_TAG" = 1 ] && tagref="+$tagref"
    if [ "$DRY_RUN" = 1 ]; then
        # --dry-run still talks to the remote, so a key without write
        # access fails here rather than on release day.
        git -C "$WORK" push --dry-run --atomic origin HEAD:main "$tagref"
        echo "  dry run: would push $(git -C "$WORK" rev-parse --short HEAD) to main and tag v$PUSH_VERSION"
        exit 0
    fi
    git -C "$WORK" push --quiet --atomic origin HEAD:main "$tagref"
    echo "  ✓ pushed and tagged v$PUSH_VERSION"
fi

[ -n "$live" ] && exit 0
cat <<NOTE

The tag starts .github/workflows/publish.yml in kevy-flutter, which
publishes to pub.dev over GitHub's OIDC token. pub.dev accepts that only
once the package's Admin tab has "Enable publishing from GitHub Actions"
on for repository \`goliajp/kevy-flutter\` with tag pattern
\`v{{version}}\`. Until then that run fails at upload, and the version is
published by hand from the tag:

    git clone --branch v$PUSH_VERSION $REPO /tmp/kevy-flutter
    cd /tmp/kevy-flutter && flutter pub publish
NOTE

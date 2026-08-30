#!/usr/bin/env python3
"""Move the version in every layer `check_version_alignment.py` checks.

A bump that moves one layer and forgets another ships a lie — the 5.1.0
bump moved Cargo and stopped, and fourteen language doors kept declaring
5.0.0. The gate exists because of that. This exists so the bump and the
gate cannot disagree about *where* a version lives: it edits exactly the
files the gate reads, matching exactly the patterns the gate matches.

Layer 6 is not here and cannot be: vendored engine bytes do not *say* a
version, they **are** one. Rebuild them (see the release skill) and let
the gate confirm.

    python3 tools/bump_version.py 5.4.0            # edit
    python3 tools/bump_version.py 5.4.0 --dry-run  # show what would change

Then, always:

    python3 tools/check_version_alignment.py
"""
from __future__ import annotations

import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
# Reuse the gate's own exclusions and independent-track list rather than
# restating them: a door the gate ignores must not be bumped either.
from check_version_alignment import (  # noqa: E402
    INDEPENDENT, INDEPENDENT_VERSION, skip,
)

VERSION_RE = re.compile(r"^\d+\.\d+\.\d+$")


def source(f: pathlib.Path, changes: list) -> str:
    """The file's text as the *next* editor should see it.

    Every bump function used to read from disk, so two of them touching one
    file each produced a full copy of the original with only their own edit
    applied — and writing both in order silently dropped the first. That is
    not hypothetical: `bindings/go/README.md` carries both a prose version
    claim and the Go module path, and it was the file that showed it.
    """
    for path, txt in reversed(changes):
        if path == f:
            return txt
    return f.read_text(encoding="utf-8")


def record(f: pathlib.Path, txt: str, changes: list) -> None:
    """Replace this file's pending text, or add it."""
    for i, (path, _) in enumerate(changes):
        if path == f:
            changes[i] = (f, txt)
            return
    changes.append((f, txt))


# Records of what happened, which layer 7 must not rewrite.
#
# A finding, a changelog entry and a completed roadmap line describe a past
# state. Moving `/v5` to `/v6` in them does not update anything — it makes
# the record false, the way relabelling a benchmark table with the current
# release turns an honest measurement into a claim about a build nobody
# ran. One of them ended up reading `go get .../kevy-go/v6@v5.1.0`, a
# command that never existed and could not have worked.
#
# Layer 7 governs what DETERMINES or INSTRUCTS the import path: code,
# manifests, scripts, and the documents that tell a reader what to import.
HISTORICAL = (
    "CHANGELOG.md",
    ".claude/ROADMAP.md",
    "bench/FINDING-",
    "bench/PERF-",
)


def historical(p) -> bool:
    rel = str(p.relative_to(ROOT))
    return any(rel == h or rel.startswith(h) for h in HISTORICAL)


def cargo_files():
    return ([ROOT / "Cargo.toml"]
            + sorted(ROOT.glob("crates/*/Cargo.toml"))
            + sorted(ROOT.glob("bindings/**/Cargo.toml")))


def bump_cargo(new: str, changes: list) -> None:
    own = re.compile(r'^(\s*version\s*=\s*")(\d+\.\d+\.\d+)(")')
    # Two corrections, both found by a 6.0.0 bump that would have shipped.
    #
    # The pattern demanded a suffix after `kevy-`, so pins on the crate the
    # project is NAMED after were never rewritten: kevy-client and
    # kevy-cluster-rw kept `kevy = "5.4.1"`, and `cargo publish` resolves a
    # version-gated dependency against crates.io — the new crate would have
    # resolved to the old sibling.
    #
    # And the value came from `new` regardless of what the pin points AT, so
    # a pin on an independent-line crate was rewritten to the workspace
    # version — naming a version of kevy-client that does not exist. The
    # target decides, exactly as the gate reads it.
    pin = re.compile(
        r'(path\s*=\s*"[^"]*?(kevy(?:-[\w-]+)?)"\s*,\s*version\s*=\s*"=?)(\d+\.\d+\.\d+)(")')

    def pin_sub(m: re.Match) -> str:
        target = f"crates/{m.group(2)}/Cargo.toml"
        want = INDEPENDENT_VERSION.get(target, new)
        return m.group(1) + want + m.group(4)
    for p in cargo_files():
        if skip(p):
            continue
        rel = str(p.relative_to(ROOT))
        out, hit = [], False
        for line in p.read_text(encoding="utf-8").splitlines(keepends=True):
            if line.lstrip().startswith("#"):
                out.append(line)
                continue
            if rel not in INDEPENDENT:
                line2 = own.sub(lambda m: m.group(1) + new + m.group(3), line, count=1)
                if line2 != line:
                    hit = True
                line = line2
            line2 = pin.sub(pin_sub, line)
            if line2 != line:
                hit = True
            out.append(line2)
        if hit:
            changes.append((p, "".join(out)))


def bump_json(new: str, changes: list) -> None:
    files = (sorted(ROOT.glob("bindings/**/package.json"))
             + sorted(ROOT.glob("packaging/**/package.json"))
             + sorted(ROOT.glob("crates/*/pkg/package.json")))
    for p in files:
        if skip(p):
            continue
        txt = p.read_text(encoding="utf-8")
        try:
            data = json.loads(txt)
        except json.JSONDecodeError:
            continue
        edited = txt
        if VERSION_RE.match(str(data.get("version", ""))):
            edited = re.sub(r'("version"\s*:\s*")\d+\.\d+\.\d+(")',
                            lambda m: m.group(1) + new + m.group(2), edited, count=1)
        for field in ("dependencies", "devDependencies", "optionalDependencies",
                      "peerDependencies"):
            for name, req in (data.get(field) or {}).items():
                if name.startswith("@goliapkg/") and VERSION_RE.match(str(req)):
                    edited = re.sub(
                        r'("' + re.escape(name) + r'"\s*:\s*")\d+\.\d+\.\d+(")',
                        lambda m: m.group(1) + new + m.group(2), edited)
        if edited != txt:
            changes.append((p, edited))


def bump_patterned(new: str, changes: list) -> None:
    """Everything the gate reads through one regex with one capture group."""
    specs = [
        ("bindings/python/pyproject.toml", r'(^version\s*=\s*")(\d+\.\d+\.\d+)(")'),
        ("bindings/flutter/pubspec.yaml", r"(^version:\s*)(\d+\.\d+\.\d+)()"),
        ("bindings/python/kevy/__init__.py", r'(^__version__\s*=\s*")(\d+\.\d+\.\d+)(")'),
        ("bindings/expo/android/build.gradle", r"(^version\s*=\s*')(\d+\.\d+\.\d+)(')"),
        ("bindings/expo/android/build.gradle", r"(versionName\s*')(\d+\.\d+\.\d+)(')"),
    ]
    pending: dict[pathlib.Path, str] = {}
    for rel, pat in specs:
        f = ROOT / rel
        if not f.exists():
            continue
        txt = pending.get(f, f.read_text(encoding="utf-8"))
        pending[f] = re.sub(pat, lambda m: m.group(1) + new + m.group(3), txt, flags=re.M)
    for f, txt in pending.items():
        if txt != f.read_text(encoding="utf-8"):
            changes.append((f, txt))

    for f in sorted(ROOT.glob("bindings/**/pom.xml")):
        if skip(f):
            continue
        txt = f.read_text(encoding="utf-8")
        edited = re.sub(r"(<version>)\d+\.\d+\.\d+(</version>)",
                        lambda m: m.group(1) + new + m.group(2), txt, count=1)
        if edited != txt:
            changes.append((f, edited))
    for f in sorted(ROOT.glob("bindings/**/*.csproj")):
        if skip(f):
            continue
        txt = f.read_text(encoding="utf-8")
        edited = re.sub(r"(<Version>)\d+\.\d+\.\d+(</Version>)",
                        lambda m: m.group(1) + new + m.group(2), txt)
        if edited != txt:
            changes.append((f, edited))


    # CMake's project() version. The gate learned this format on 2026-08-30
    # after bindings/cpp sat three releases behind, unnoticed because
    # nothing consumes it. A gate that reads a layer and a bump that does
    # not is the same hole with an extra step.
    for f in sorted(ROOT.glob("bindings/**/CMakeLists.txt")):
        if skip(f):
            continue
        txt = source(f, changes)
        edited = re.sub(r"(^project\([^)]*?VERSION\s+)\d+\.\d+\.\d+",
                        lambda m: m.group(1) + new, txt, flags=re.M | re.S)
        if edited != txt:
            record(f, edited, changes)


def bump_prose(new: str, changes: list) -> None:
    claim = re.compile(
        r"(tracks kevy \*\*)(\d+\.\d+\.\d+)(\*\*)"
        r"|(`jp\.golia:kevy:)(\d+\.\d+\.\d+)(`)"
        r"|(<artifactId>kevy</artifactId><version>)(\d+\.\d+\.\d+)(</version>)"
        # The three bindings tables — one per language — state a version per
        # row. The gate has read them since it learned to; this did not, so
        # the 6.1.0 bump left twenty-four rows saying 6.0.0 and the gate
        # caught it. Same shape as every other layer that bit alone.
        r"|(^\|[^|]*\|[^|]*\|\s*)(\d+\.\d+\.\d+)(\s*\|$)", re.M)

    def sub(m: re.Match) -> str:
        g = [x for x in m.groups() if x is not None]
        return g[0] + new + g[2]

    # Exactly the files layer 5 of the gate reads — the whole point of this
    # tool is that the two cannot disagree about where a version lives.
    for f in (sorted(ROOT.glob("bindings/**/*.md"))
              + [ROOT / "README.md", ROOT / "docs/bindings.md",
                 ROOT / "docs/ja/bindings.md", ROOT / "docs/zh/bindings.md"]):
        if skip(f) or not f.exists():
            continue
        txt = source(f, changes)
        edited = claim.sub(sub, txt)
        if edited != txt:
            record(f, edited, changes)


def bump_go_module_major(new: str, changes: list) -> None:
    """Move `kevy-go/vN` when the major moves.

    Go puts the major in the import path for major >= 2, so a major bump
    that leaves `/v5` behind produces a module that resolves to the wrong
    major forever. `scripts/mirror-go-module.sh` refuses the mismatch, but
    it runs after the tag — and after crates.io has published.

    A bare `github.com/goliajp/kevy-go` with no suffix is the repository,
    not the module, and is left alone. Below major 2 Go uses no suffix at
    all, which is a migration this cannot do mechanically, so it says so
    rather than guessing.
    """
    major = int(new.split(".")[0])
    if major < 2:
        return
    used = re.compile(r"(github\.com/goliajp/kevy-go)/v\d+")
    want = rf"\1/v{major}"
    for f in sorted(ROOT.glob("**/*")):
        if f.is_dir() or skip(f) or historical(f) or f.suffix not in (
                ".go", ".mod", ".sh", ".md", ".yml", ".yaml"):
            continue
        try:
            txt = source(f, changes)
        except (OSError, UnicodeDecodeError):
            continue
        edited = used.sub(want, txt)
        if edited != txt:
            record(f, edited, changes)


def main() -> int:
    args = [a for a in sys.argv[1:] if not a.startswith("-")]
    dry = "--dry-run" in sys.argv
    if len(args) != 1 or not VERSION_RE.match(args[0]):
        print(__doc__)
        return 2
    new = args[0]

    changes: list[tuple[pathlib.Path, str]] = []
    bump_cargo(new, changes)
    bump_json(new, changes)
    bump_patterned(new, changes)
    bump_prose(new, changes)
    bump_go_module_major(new, changes)

    if not changes:
        print(f"bump: nothing to change — every layer already reads {new}")
        return 0
    for p, txt in changes:
        print(f"  {p.relative_to(ROOT)}")
        if not dry:
            p.write_text(txt, encoding="utf-8")
    verb = "would edit" if dry else "edited"
    print(f"\nbump: {verb} {len(changes)} files to {new}.")
    print("Layer 6 (vendored engine bytes) is NOT bumped here — rebuild it.")
    print("Layer 7 (the Go module's major, which lives in the import path) IS.")
    print("Then run: python3 tools/check_version_alignment.py")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

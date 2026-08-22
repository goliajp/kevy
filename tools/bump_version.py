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
from check_version_alignment import INDEPENDENT, skip  # noqa: E402

VERSION_RE = re.compile(r"^\d+\.\d+\.\d+$")


def cargo_files():
    return ([ROOT / "Cargo.toml"]
            + sorted(ROOT.glob("crates/*/Cargo.toml"))
            + sorted(ROOT.glob("bindings/**/Cargo.toml")))


def bump_cargo(new: str, changes: list) -> None:
    own = re.compile(r'^(\s*version\s*=\s*")(\d+\.\d+\.\d+)(")')
    pin = re.compile(r'(path\s*=\s*"[^"]*kevy-[\w-]+"\s*,\s*version\s*=\s*"=?)(\d+\.\d+\.\d+)(")')
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
            line2 = pin.sub(lambda m: m.group(1) + new + m.group(3), line)
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


def bump_prose(new: str, changes: list) -> None:
    claim = re.compile(
        r"(tracks kevy \*\*)(\d+\.\d+\.\d+)(\*\*)"
        r"|(`jp\.golia:kevy:)(\d+\.\d+\.\d+)(`)"
        r"|(<artifactId>kevy</artifactId><version>)(\d+\.\d+\.\d+)(</version>)")

    def sub(m: re.Match) -> str:
        g = [x for x in m.groups() if x is not None]
        return g[0] + new + g[2]

    for f in sorted(ROOT.glob("bindings/**/*.md")) + [ROOT / "README.md"]:
        if skip(f) or not f.exists():
            continue
        txt = f.read_text(encoding="utf-8")
        edited = claim.sub(sub, txt)
        if edited != txt:
            changes.append((f, edited))


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
    print("Then run: python3 tools/check_version_alignment.py")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

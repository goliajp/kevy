#!/usr/bin/env python3
"""Every version-bearing file in this repository agrees with the workspace.

A kevy release moves the version in SIX distinct layers, and a bump that
moves one and forgets another is a release that ships a lie. The 5.1.0
bump moved layer 1 and stopped: fourteen language bindings still
declared 5.0.0, two of them in vendored engine BYTES — and 5.0.0 has a
compression defect that can make a stored value unreadable, so those
doors would have shipped the hazard the release existed to close.

The layers, and why each one bites on its own:

1. Cargo — the workspace version plus every `path = "../kevy-x",
   version = "…"` pin, including exact `=` pins and hand-aligned
   whitespace. A stale pin resolves a 5.1 crate against a 5.0 sibling.
2. Language manifests — npm, pyproject, pubspec, csproj, gradle. What
   a package manager stamps on the artifact.
3. Inter-package pins — our own `@goliapkg/*` referring to each other.
   Miss these and a 5.1 door installs 5.0 companions.
4. Live constants — `__version__`, gradle `versionName`. Code, not
   metadata: they answer at runtime.
5. Version claims in prose — "this door tracks kevy X". A reader
   trusts these more than the manifest.
6. Vendored engine bytes — jniLibs, xcframework. These do not merely
   SAY a version, they ARE one; the engine's C ABI self-reports it, so
   `strings` settles the question.

Run: python3 tools/check_version_alignment.py
"""

import json
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

# Packages on their own version track, with the reason. An entry here is
# a claim that the file is SUPPOSED to disagree with the workspace.
INDEPENDENT = {
    "crates/kevy-client/Cargo.toml": "the sync client keeps its own 2.x line",
    "crates/kevy-client-async/Cargo.toml": "twin of kevy-client, same line",
}

# Example / demo applications that ship with a binding. They version
# themselves as sample code and are not products.
EXAMPLE_APPS = (
    "example/",
    "barern-example/",
    "smoke/",
)

# Files whose 5.0.0-shaped strings belong to somebody else.
THIRD_PARTY = ("node_modules", "package-lock.json", "/target/", "/.build/", "Cargo.lock")

VERSION_RE = re.compile(r"^\d+\.\d+\.\d+$")


def workspace_version() -> str:
    txt = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    m = re.search(r'^version = "(\d+\.\d+\.\d+)"', txt, re.M)
    if not m:
        sys.exit("check_version_alignment: no workspace version in Cargo.toml")
    return m.group(1)


def skip(path: pathlib.Path) -> bool:
    s = str(path)
    return any(t in s for t in THIRD_PARTY) or any(e in s for e in EXAMPLE_APPS)


def layer1_cargo(v: str, bad: list) -> int:
    """Workspace version + every version-gated path dependency."""
    checked = 0
    for p in [ROOT / "Cargo.toml"] + sorted(ROOT.glob("crates/*/Cargo.toml")) + sorted(
        ROOT.glob("bindings/**/Cargo.toml")
    ):
        if skip(p):
            continue
        rel = str(p.relative_to(ROOT))
        txt = p.read_text(encoding="utf-8")
        for i, line in enumerate(txt.splitlines(), 1):
            if line.lstrip().startswith("#"):
                continue
            # A crate's own version.
            m = re.match(r'^version\s*=\s*"(\d+\.\d+\.\d+)"', line.strip())
            if m:
                checked += 1
                if m.group(1) != v and rel not in INDEPENDENT:
                    bad.append(f"{rel}:{i}: own version {m.group(1)} != {v}")
                continue
            # A version-gated path dependency on a sibling.
            m = re.search(
                r'path\s*=\s*"[^"]*kevy-[\w-]+"\s*,\s*version\s*=\s*"=?(\d+\.\d+\.\d+)"',
                line,
            )
            if m:
                checked += 1
                if m.group(1) != v:
                    bad.append(f"{rel}:{i}: path-dep pin {m.group(1)} != {v}")
    return checked


def layer23_manifests(v: str, bad: list) -> int:
    """Language manifests, plus our own inter-package pins inside them."""
    checked = 0
    for p in sorted(ROOT.glob("bindings/**/package.json")) + sorted(
        ROOT.glob("packaging/**/package.json")
    ) + sorted(ROOT.glob("crates/*/pkg/package.json")):
        if skip(p):
            continue
        rel = str(p.relative_to(ROOT))
        try:
            data = json.loads(p.read_text(encoding="utf-8"))
        except json.JSONDecodeError as e:
            bad.append(f"{rel}: unreadable ({e})")
            continue
        if VERSION_RE.match(str(data.get("version", ""))):
            checked += 1
            if data["version"] != v:
                bad.append(f"{rel}: version {data['version']} != {v}")
        for field in ("dependencies", "devDependencies", "optionalDependencies",
                      "peerDependencies"):
            for name, req in (data.get(field) or {}).items():
                if name.startswith("@goliapkg/") and VERSION_RE.match(str(req)):
                    checked += 1
                    if req != v:
                        bad.append(f"{rel}: {field}.{name} pinned {req} != {v}")

    for p, pat in (
        ("bindings/python/pyproject.toml", r'^version\s*=\s*"(\d+\.\d+\.\d+)"'),
        ("bindings/flutter/pubspec.yaml", r"^version:\s*(\d+\.\d+\.\d+)"),
    ):
        f = ROOT / p
        if not f.exists():
            continue
        m = re.search(pat, f.read_text(encoding="utf-8"), re.M)
        if m:
            checked += 1
            if m.group(1) != v:
                bad.append(f"{p}: {m.group(1)} != {v}")

    # Maven poms. Their absence from this gate is how the Java door sat at
    # 5.0.0 through a release that moved everything else — the gate could
    # only see the formats it had been taught.
    for f in sorted(ROOT.glob("bindings/**/pom.xml")):
        if skip(f):
            continue
        m = re.search(r"<version>(\d+\.\d+\.\d+)</version>", f.read_text(encoding="utf-8"))
        if m:
            checked += 1
            if m.group(1) != v:
                bad.append(f"{f.relative_to(ROOT)}: <version> {m.group(1)} != {v}")

    for f in sorted(ROOT.glob("bindings/**/*.csproj")):
        if skip(f):
            continue
        m = re.search(r"<Version>(\d+\.\d+\.\d+)</Version>", f.read_text(encoding="utf-8"))
        if m:
            checked += 1
            if m.group(1) != v:
                bad.append(f"{f.relative_to(ROOT)}: <Version> {m.group(1)} != {v}")
    return checked


def layer4_live_constants(v: str, bad: list) -> int:
    """Values the code answers with at runtime."""
    checked = 0
    probes = [
        ("bindings/python/kevy/__init__.py", r'^__version__\s*=\s*"(\d+\.\d+\.\d+)"'),
        ("bindings/expo/android/build.gradle", r"^version\s*=\s*'(\d+\.\d+\.\d+)'"),
        ("bindings/expo/android/build.gradle", r"versionName\s*'(\d+\.\d+\.\d+)'"),
    ]
    for rel, pat in probes:
        f = ROOT / rel
        if not f.exists():
            continue
        for m in re.finditer(pat, f.read_text(encoding="utf-8"), re.M):
            checked += 1
            if m.group(1) != v:
                bad.append(f"{rel}: live constant {m.group(1)} != {v}")
    return checked


def layer5_prose(v: str, bad: list) -> int:
    """"This door tracks kevy X" — a claim a reader trusts."""
    checked = 0
    # The third alternative is the copy-pasteable Maven coordinate. A
    # reader does not "trust" that one — they paste it, and a stale
    # version resolves to a real older artifact rather than erroring.
    claim = re.compile(
        r"tracks kevy \*\*(\d+\.\d+\.\d+)\*\*"
        r"|`jp\.golia:kevy:(\d+\.\d+\.\d+)`"
        r"|<artifactId>kevy</artifactId><version>(\d+\.\d+\.\d+)</version>")
    # The root README is in scope and was not: it is the most-read file
    # here, and its install block states versions. docs/ stays out on
    # purpose — the upgrade guides name old versions correctly, and a
    # gate that dragged those forward would be rewriting history.
    files = sorted(ROOT.glob("bindings/**/*.md")) + [ROOT / "README.md"]
    for f in files:
        if skip(f):
            continue
        for i, line in enumerate(f.read_text(encoding="utf-8").splitlines(), 1):
            m = claim.search(line)
            if m:
                found = m.group(1) or m.group(2) or m.group(3)
                checked += 1
                if found != v:
                    bad.append(f"{f.relative_to(ROOT)}:{i}: claims {found} != {v}")
    return checked


def layer6_vendored_bytes(v: str, bad: list) -> int:
    """The artifacts that do not say a version — they are one."""
    checked = 0
    natives = [
        p for p in ROOT.glob("bindings/**/*")
        if (p.suffix == ".so" and "jniLibs" in str(p))
        or (p.suffix == ".a" and "xcframework" in str(p))
    ]
    for p in sorted(set(natives)):
        if skip(p):
            continue
        try:
            # Bytes, not text: an object file is not UTF-8 and decoding it
            # crashed the first version of this check.
            out = subprocess.run(["strings", str(p)], capture_output=True,
                                 timeout=180).stdout
        except (OSError, subprocess.SubprocessError) as e:
            bad.append(f"{p.relative_to(ROOT)}: could not read strings ({e})")
            continue
        found = sorted({
            line.decode("ascii") for line in out.splitlines()
            if VERSION_RE.match(line.decode("ascii", "ignore"))
        })
        checked += 1
        if v not in found:
            bad.append(
                f"{p.relative_to(ROOT)}: engine bytes self-report {found or ['nothing']}, "
                f"not {v} — rebuild and re-vendor (see the release skill)"
            )
    return checked


def main() -> int:
    v = workspace_version()
    bad: list[str] = []
    counts = {
        "cargo": layer1_cargo(v, bad),
        "manifests+pins": layer23_manifests(v, bad),
        "live constants": layer4_live_constants(v, bad),
        "prose claims": layer5_prose(v, bad),
        "vendored bytes": layer6_vendored_bytes(v, bad),
    }
    # A layer that finds nothing has not verified anything. Without this
    # the gate would go green on a checkout where the vendored artifacts
    # were never built, or after someone untracks them — the same empty-
    # predicate failure this project writes rules about. Floors are the
    # minimum a bare checkout must contain, not a target.
    floors = {"cargo": 40, "manifests+pins": 10, "live constants": 3,
              "prose claims": 3, "vendored bytes": 2}
    for layer, floor in floors.items():
        if counts[layer] < floor:
            bad.append(
                f"layer '{layer}' found only {counts[layer]} declaration(s), "
                f"expected at least {floor} — the layer verified nothing, "
                f"which is not the same as everything agreeing"
            )

    if bad:
        print(f"REFUSED: {len(bad)} file(s) disagree with the workspace version {v}")
        for b in bad:
            print(f"  {b}")
        print("\nA bump that moves one layer and forgets another ships a lie.")
        print("See .claude/skills/release/SKILL.md for the six layers and the fix.")
        return 1
    total = sum(counts.values())
    detail = ", ".join(f"{k} {n}" for k, n in counts.items())
    print(f"ok: {total} version declarations all at {v} ({detail})")
    return 0


if __name__ == "__main__":
    sys.exit(main())

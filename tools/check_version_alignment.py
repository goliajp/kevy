#!/usr/bin/env python3
"""Every version-bearing file in this repository agrees with the workspace.

A kevy release moves the version in SEVEN distinct layers, and a bump that
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
7. The Go module's major — Go puts the major version in the IMPORT
   PATH (`/v6`), so a module that forgets it does not merely say the
   wrong number, it resolves to the wrong major forever. Added in
   6.0.0, which is why everything else here still said six.

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
def _independent_versions():
    """The version each independently-lined crate actually declares.

    Read from the crate rather than restated here: a second copy of a
    version number is one more place for a bump to forget, which is the
    whole defect this gate exists to catch.
    """
    out = {}
    for rel in INDEPENDENT:
        try:
            txt = (ROOT / rel).read_text(encoding="utf-8")
        except OSError:
            continue
        m = re.search(r'^version\s*=\s*"(\d+\.\d+\.\d+)"', txt, re.M)
        if m:
            out[rel] = m.group(1)
    return out


INDEPENDENT = {
    "crates/kevy-client/Cargo.toml": "the sync client keeps its own 2.x line",
    "crates/kevy-client-async/Cargo.toml": "twin of kevy-client, same line",
}

INDEPENDENT_VERSION = _independent_versions()

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
            #
            # The pin must match the version line of the crate it POINTS AT,
            # which is not always the workspace version: kevy-client and its
            # async twin keep their own 2.x line (see INDEPENDENT above), so a
            # correct pin on them reads 2.2.0 and a pin reading 5.4.1 would
            # name a version that does not exist.
            #
            # Until v6 those pins carried no version at all — a path-only
            # dev-dependency, invisible to this check and dropped outright by
            # `cargo package`, which is how four published stones came to ship
            # tests importing a crate their manifest never declared. Giving
            # them versions is what made this case reachable.
            # `kevy-[\w-]+` required a suffix, so every pin on the crate the
            # project is NAMED after — `path = "../kevy"` — was invisible to
            # this gate. Two carried it: kevy-client and kevy-cluster-rw. A
            # stale pin there is the exact hazard layer 1 is about, because
            # `cargo publish` resolves a version-gated dependency against
            # crates.io: the new crate would have resolved to the old sibling.
            m = re.search(
                r'path\s*=\s*"[^"]*?(kevy(?:-[\w-]+)?)"\s*,\s*version\s*=\s*"=?(\d+\.\d+\.\d+)"',
                line,
            )
            if m:
                checked += 1
                target = f"crates/{m.group(1)}/Cargo.toml"
                want = INDEPENDENT_VERSION.get(target, v)
                if m.group(2) != want:
                    where = "" if want == v else f" (independent line: {target})"
                    bad.append(f"{rel}:{i}: path-dep pin {m.group(2)} != {want}{where}")
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
        r"|<artifactId>kevy</artifactId><version>(\d+\.\d+\.\d+)</version>"
        # The install table's Version column. Three Maven lines came into
        # scope above and the seven rows above them did not — the table is
        # the part a reader reads first, and it was the part that was stale.
        r"|^\|[^|]*\|[^|]*\|\s*(\d+\.\d+\.\d+)\s*\|$")
    # The root README is in scope and was not: it is the most-read file
    # here, and its install block states versions. docs/ stays out on
    # purpose — the upgrade guides name old versions correctly, and a
    # gate that dragged those forward would be rewriting history.
    #
    # `docs/bindings.md` is the exception to that exception, and its three
    # translations with it. It is not history: it is an install table under
    # the sentence "every line here was installed from its registry and run
    # before it was written down", and it carried 5.1.0 in all seven rows
    # and in the same Maven XML the README states at 6.0.0. A reader pastes
    # this one. The blanket docs/ exclusion is right for what it was written
    # for and was covering this too.
    files = (sorted(ROOT.glob("bindings/**/*.md"))
             + [ROOT / "README.md", ROOT / "docs/bindings.md",
                ROOT / "docs/ja/bindings.md", ROOT / "docs/zh/bindings.md"])
    for f in files:
        if skip(f):
            continue
        for i, line in enumerate(f.read_text(encoding="utf-8").splitlines(), 1):
            m = claim.search(line)
            if m:
                found = m.group(1) or m.group(2) or m.group(3) or m.group(4)
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


def layer7_go_module_major(v: str, bad: list) -> int:
    """Go's major lives in the import path, and nothing checked it.

    `import "github.com/goliajp/kevy-go/v5"` is not a version *string* that
    happens to appear in a document — for major >= 2 the path IS the major,
    the way a vendored `.so` IS one. Get it wrong and the module resolves to
    the wrong major forever.

    `scripts/mirror-go-module.sh` refuses a mismatch, but it runs AFTER the
    tag: crates.io has published by then, and that cannot be undone. So the
    check belongs here, in precommit, where a major bump that forgot the
    path fails before anything irreversible happens.

    Two rules, because the same string means two things. A bare
    `github.com/goliajp/kevy-go` is the *repository*, which has no major and
    must not be flagged; with a `/vN` it is the *module path*. So: every
    `/vN` that appears must match, and the two places that declare the
    module — the mirror script and `bindings/go/go.mod` — must carry one.
    """
    major = int(v.split(".")[0])
    if major < 2:
        return 0
    want = f"/v{major}"
    used = re.compile(r"github\.com/goliajp/kevy-go/v(\d+)")
    checked = 0
    for pth in sorted(ROOT.glob("**/*")):
        if pth.is_dir() or skip(pth) or historical(pth) or pth.suffix not in (
                ".go", ".mod", ".sh", ".md", ".yml", ".yaml"):
            continue
        try:
            txt = pth.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        for m in used.finditer(txt):
            checked += 1
            if m.group(0).endswith(want):
                continue
            bad.append(
                f"{pth.relative_to(ROOT)}: Go module path says '{m.group(0)}' "
                f"but the workspace is {v} — Go puts the major in the path, "
                f"so this resolves to the wrong major forever"
            )
    for rel in ("scripts/mirror-go-module.sh", "bindings/go/go.mod"):
        pth = ROOT / rel
        if not pth.exists():
            bad.append(f"{rel}: missing — the Go module's major is declared here")
            continue
        if want not in pth.read_text(encoding="utf-8"):
            bad.append(
                f"{rel}: declares the Go module without '{want}'. This is where "
                f"the major is set; the mirror script refuses a mismatch, but "
                f"only after the tag has already published to crates.io"
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
        "go module major": layer7_go_module_major(v, bad),
    }
    # A layer that finds nothing has not verified anything. Without this
    # the gate would go green on a checkout where the vendored artifacts
    # were never built, or after someone untracks them — the same empty-
    # predicate failure this project writes rules about. Floors are the
    # minimum a bare checkout must contain, not a target.
    floors = {"cargo": 40, "manifests+pins": 10, "live constants": 3,
              "prose claims": 3, "vendored bytes": 2,
              # Nine files carry the Go import path; a run that finds none
              # has stopped looking, which is how this layer went unchecked
              # in the first place.
              "go module major": 5}
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
        print("See .claude/skills/release/SKILL.md for the seven layers and the fix.")
        return 1
    total = sum(counts.values())
    detail = ", ".join(f"{k} {n}" for k, n in counts.items())
    # "all at 5.4.1" stopped being true the moment pins on the independent
    # 2.x client line became visible to this gate. A summary that overstates
    # what it checked is the same kind of lie the gate exists to catch.
    if INDEPENDENT_VERSION:
        lines = ", ".join(f"{k.split('/')[1]} at {ver}"
                          for k, ver in sorted(INDEPENDENT_VERSION.items()))
        print(f"ok: {total} version declarations consistent — workspace at {v}, "
              f"{lines} ({detail})")
    else:
        print(f"ok: {total} version declarations all at {v} ({detail})")
    return 0


if __name__ == "__main__":
    sys.exit(main())

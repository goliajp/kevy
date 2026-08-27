#!/usr/bin/env python3
"""A published crate must not ship a file it cannot compile.

Found by the stone sandbox in four crates and then measured across the
workspace: **fifteen published crates shipped 122 source files importing a
dependency their published manifest does not declare.** `cargo package`
drops a dev-dependency written as `{ path = "..." }` with no version, and
packages the sources that need it anyway — so the crate arrives on
crates.io carrying tests that fail to compile the moment anyone unpacks it
and runs them.

Nothing caught it because every other check looks at the crate INSIDE the
workspace, where `../kevy-bench` resolves.

Two ways to satisfy this gate, and the layer decides which:

- **Version the dependency.** Then `cargo package` keeps it and the file
  compiles. This is what a stone does: it promises "any project could take
  these", and that promise includes its tests.
- **Exclude the file.** A server or CLI crate does not promise "take it and
  run my integration suite" — that suite needs a test network, a chaos
  harness and the CLI itself. Excluding is the honest answer there, and it
  is not the same as hiding: the file still runs in the workspace.

The packaged set is computed from git rather than from `cargo package
--list`, which needs every dependency resolvable on the registry and so
cannot run while a new crate is still being published for the first time.

Floor rule: finding no publishable crates, or no tracked files, is a
failure of the selector rather than a pass.

Run: python3 tools/check_package.py
Exit: 0 pass, 1 violation, 2 refused.
"""

import fnmatch
import pathlib
import re
import subprocess
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
CRATES = ROOT / "crates"
MIN_CRATES = 30

# Blank comments and strings before looking for imports: `//! Arbitrary
# bytes handed to Seg::open` is prose, not a use of the `arbitrary` crate,
# and three such lines nearly cost a real dependency its removal.
SKIP = re.compile(r"""
      (?P<line_comment>//[^\n]*)
    | (?P<block_comment>/\*.*?\*/)
    | (?P<string>"(?:\\.|[^"\\])*")
    | (?P<raw>r\#*"(?:.|\n)*?"\#*)
""", re.VERBOSE | re.DOTALL)


def refuse(msg):
    print(f"packagegate: REFUSED — {msg}", file=sys.stderr)
    sys.exit(2)


def tracked():
    try:
        out = subprocess.run(["git", "ls-files"], cwd=ROOT,
                             capture_output=True, text=True, check=True).stdout
    except (OSError, subprocess.CalledProcessError) as e:
        refuse(f"git ls-files failed: {e}")
    files = out.splitlines()
    if len(files) < 100:
        refuse(f"git ls-files returned {len(files)} paths; this is not the tree")
    return files


def manifests():
    out = {}
    for f in sorted(CRATES.glob("*/Cargo.toml")):
        doc = tomllib.loads(f.read_text())
        pkg = doc.get("package", {})
        if pkg.get("publish") is False:
            continue
        out[pkg["name"]] = (f.parent, doc)
    if len(out) < MIN_CRATES:
        refuse(f"found {len(out)} publishable crates; the selector is broken")
    return out


def declared(doc):
    """Dependencies that survive `cargo package` into the published manifest."""
    keep = set()
    for section in ("dependencies", "build-dependencies"):
        keep |= set(doc.get(section, {}))
    for name, spec in doc.get("dev-dependencies", {}).items():
        # A path-only dev-dependency is dropped outright.
        if not isinstance(spec, dict) or "path" not in spec or "version" in spec:
            keep.add(name)
    for tgt in doc.get("target", {}).values():
        for section in ("dependencies", "build-dependencies", "dev-dependencies"):
            keep |= set(tgt.get(section, {}))
    return keep


def packaged(cratedir, doc, tracked_files):
    """Git-tracked .rs files under the crate, minus its `exclude` patterns."""
    rel = str(cratedir.relative_to(ROOT))
    excl = doc.get("package", {}).get("exclude", [])
    out = []
    for f in tracked_files:
        if not f.startswith(rel + "/") or not f.endswith(".rs"):
            continue
        inner = f[len(rel) + 1:]
        if any(fnmatch.fnmatch(inner, p) or inner.startswith(p.rstrip("*").rstrip("/") + "/")
               for p in excl):
            continue
        out.append((inner, ROOT / f))
    return out


def imports(path, family):
    """Workspace crates this file actually uses, comments and strings aside."""
    try:
        text = path.read_text(errors="replace")
    except OSError:
        return set()
    code = SKIP.sub(lambda m: re.sub(r"[^\n]", " ", m.group()), text)
    found = set()
    for name in family:
        mod = name.replace("-", "_")
        if re.search(rf"\buse\s+{mod}\b|\b{mod}\s*::", code):
            found.add(name)
    return found


def main():
    tracked_files = tracked()
    mans = manifests()
    family = {p.name for p in CRATES.iterdir() if (p / "Cargo.toml").exists()}

    bad, checked = [], 0
    for name, (cratedir, doc) in sorted(mans.items()):
        keep = declared(doc)
        for inner, path in packaged(cratedir, doc, tracked_files):
            checked += 1
            missing = imports(path, family) - keep - {name}
            for dep in sorted(missing):
                bad.append(f"{name}: ships {inner}, which uses `{dep}` — "
                           f"not in the published manifest")
    if not checked:
        refuse("no packaged source files examined; the selector is broken")

    if bad:
        print(f"packagegate: FAIL — {len(bad)} shipped file(s) import a "
              f"dependency the published manifest drops")
        for b in bad[:30]:
            print(f"  {b}")
        if len(bad) > 30:
            print(f"  … and {len(bad) - 30} more")
        print("  fix: give the dependency a version, or exclude the file")
        return 1
    print(f"packagegate: PASS — {checked} packaged files across {len(mans)} "
          f"publishable crates, every import declared")
    return 0


if __name__ == "__main__":
    sys.exit(main())

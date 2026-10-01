#!/usr/bin/env python3
"""Rows whose inputs have not changed since they last passed here.

The heavy rows above premerge each measure one subsystem — the text index,
the vector index, the replay path — and a release re-ran every one of them
whether or not a line of that subsystem had changed since it last passed on
this machine. A row that declares `inputs` (path prefixes ending in `/`, or
fnmatch patterns, relative to the repository root) is skipped as UNCHANGED
when all of these hold:

- the working tree has no tracked changes, so HEAD is what would run;
- this machine recorded the row passing at commit P, P is an ancestor of
  HEAD, and nothing has failed it here since;
- no file changed between P and HEAD matches an input, the row's own
  command files included.

A manifest change that only raises version numbers does not count as a
change — every release bump touches every crate's Cargo.toml and the
lockfile, and a version string is not what these rows measure. Any other
edit to those files does count.

A row without `inputs` always runs; forgetting to declare one costs time,
never coverage. `--rerun-unchanged` runs everything.
"""

import fnmatch
import json
import re
import subprocess

PASSES = "target/suite.passes.json"  # not suite-*.json: that glob is the tier ledgers
VERSION = re.compile(r'\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?')
MANIFESTS = ("Cargo.toml", "Cargo.lock")


def _git(root, *args):
    r = subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True)
    return r.returncode, r.stdout


def clean_head(root):
    code, sha = _git(root, "rev-parse", "HEAD")
    _, dirty = _git(root, "status", "--porcelain", "--untracked-files=no")
    return sha.strip() if code == 0 and not dirty.strip() else None


def _load(root):
    path = root / PASSES
    return json.loads(path.read_text()) if path.exists() else {}


def record(root, results):
    """Remember which rows passed on this commit and forget the ones that
    did not; a row run on a dirty tree answers for no commit."""
    sha = clean_head(root)
    passes = _load(root)
    for c, status, *_ in results:
        if status == "PASS" and sha:
            passes[c["id"]] = sha
        elif status in ("FAIL", "TIMEOUT", "ADVISORY"):
            passes.pop(c["id"], None)
    (root / PASSES).parent.mkdir(exist_ok=True)
    (root / PASSES).write_text(json.dumps(passes, indent=1, sort_keys=True))


def _only_versions_moved(root, base, path):
    _, diff = _git(root, "diff", "-U0", base, "HEAD", "--", path)
    removed, added = [], []
    for line in diff.splitlines():
        if line.startswith(("---", "+++")):
            continue
        if line.startswith("-"):
            removed.append(VERSION.sub("V", line[1:]))
        elif line.startswith("+"):
            added.append(VERSION.sub("V", line[1:]))
    return sorted(removed) == sorted(added)


def _changed(root, base):
    _, names = _git(root, "diff", "--name-only", base, "HEAD")
    return [p for p in names.splitlines()
            if not (p.rsplit("/", 1)[-1] in MANIFESTS and _only_versions_moved(root, base, p))]


def _patterns(root, c):
    own = [tok for tok in c["cmd"].split() if "/" in tok and (root / tok).is_file()]
    return list(c["inputs"]) + own


def _hit(path, patterns):
    return any(path.startswith(p) if p.endswith("/") else fnmatch.fnmatch(path, p)
               for p in patterns)


def unchanged(root, checks):
    """`{check id: reason}` for the rows that may be skipped."""
    head = clean_head(root)
    if head is None:
        return {}
    passes = _load(root)
    diffs, out = {}, {}
    for c in checks:
        base = passes.get(c["id"])
        if not c.get("inputs") or not base:
            continue
        if base not in diffs:
            code, _ = _git(root, "merge-base", "--is-ancestor", base, head)
            diffs[base] = _changed(root, base) if code == 0 else None
        changed = diffs[base]
        if changed is None:
            continue
        pats = _patterns(root, c)
        if not any(_hit(p, pats) for p in changed):
            out[c["id"]] = f"passed here at {base[:9]}; none of its inputs changed since"
    return out


def dead_patterns(root, checks):
    """Input patterns that match no tracked file. One of those would leave
    its row skipped forever, so the audit refuses it."""
    _, names = _git(root, "ls-files")
    tracked = names.splitlines()
    return [(c["id"], p) for c in checks for p in c.get("inputs", [])
            if not any(_hit(f, [p]) for f in tracked)]

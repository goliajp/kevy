#!/usr/bin/env python3
"""Workflow actions stay on their latest stable major, and agree with each other.

Two things go wrong on their own, and both were true here:

**Drift between files.** setup-node was pinned at v4, v5 and v6
simultaneously across the workflows, so a fix applied to one job did not
apply to the next. A version used in three places is three versions.

**Falling behind.** GitHub retires the Node runtime an action major is
built on, and every job then prints a deprecation warning until the day
it stops running. Fourteen jobs were warning about Node 20; setup-java v4
had already been announced as receiving no further updates.

The check has two modes. By default it only verifies internal
consistency, which needs no network and is what CI runs. With --online it
also asks each action's own releases for the latest major, which is how
the pins get refreshed — and is not in CI, because a gate that fails when
somebody else publishes a release fails for a reason this repository
cannot act on at that moment.

Run: python3 tools/check_action_versions.py [--online]
"""

import json
import pathlib
import re
import subprocess
import sys
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
WORKFLOWS = ROOT / ".github/workflows"

USES = re.compile(r"uses:\s+([\w.-]+/[\w.-]+)@v(\d+)")


def used():
    """{action: {major: [files]}} across every workflow."""
    out = {}
    for f in sorted(WORKFLOWS.glob("*.yml")):
        for action, major in USES.findall(f.read_text(encoding="utf-8")):
            out.setdefault(action, {}).setdefault(int(major), []).append(f.name)
    return out


def latest(action):
    """The latest release's major, from the action's own repository."""
    try:
        r = subprocess.run(
            ["gh", "api", f"repos/{action}/releases/latest", "-q", ".tag_name"],
            capture_output=True, text=True, timeout=20,
        )
        tag = r.stdout.strip()
    except Exception:
        tag = ""
    if not tag:
        try:
            with urllib.request.urlopen(
                f"https://api.github.com/repos/{action}/releases/latest", timeout=20
            ) as resp:
                tag = json.load(resp).get("tag_name", "")
        except Exception:
            return None
    m = re.match(r"v?(\d+)", tag)
    return int(m.group(1)) if m else None


def main():
    online = "--online" in sys.argv
    actions = used()
    if not actions:
        sys.exit("check_action_versions: found no actions at all — the glob is wrong")

    bad = []

    # ── one major per action, everywhere ─────────────────────────────────
    for action, majors in sorted(actions.items()):
        if len(majors) > 1:
            detail = ", ".join(
                f"v{m} in {', '.join(sorted(set(files)))}" for m, files in sorted(majors.items())
            )
            bad.append(f"{action} is pinned at {len(majors)} different majors — {detail}")

    if online:
        for action, majors in sorted(actions.items()):
            have = max(majors)
            want = latest(action)
            if want is None:
                print(f"  ? {action}: could not read its releases")
                continue
            if have < want:
                bad.append(f"{action} is on v{have}; the latest stable is v{want}")

    if bad:
        print(f"check_action_versions: FAIL — {len(bad)} problem(s)\n")
        for b in bad:
            print(f"  ✗ {b}")
        print("\nPin every workflow to the same major, and keep it current:")
        print("  python3 tools/check_action_versions.py --online")
        sys.exit(1)

    n = sum(len(f) for m in actions.values() for f in m.values())
    print(
        f"ok: {n} action references across {len(actions)} actions, "
        f"one major each{' and each the latest stable' if online else ''}"
    )


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""The recorded durations are durations.

`target/suite-*.json` is what anyone reads to decide what to optimise — three
of this week's performance targets were chosen by sorting it. At least three
of its rows were not what they claimed:

  version-alignment  120.1 s   a TIMEOUT KILL recorded in the duration field
  workspace-tests   3297.0 s   16x the same command's real cold CI run (227 s)
  doctest-run        540.8 s   18x its CI cost (30 s)

while doc-toml's 65.8 s matched CI's 62 s exactly. An instrument some of whose
rows are off by an order of magnitude, with nothing in the file separating
those from the good ones, is the shape this repository calls "a measuring
device that fails in the shape of data" — applied to the device used to choose
what to measure.

This refuses a ledger that cannot tell the two apart, and flags rows whose
recorded cost is wildly out of line with the budget they declared, so a
suspect row is visible before someone spends a week on it.

Run: python3 tools/check_suite_ledger.py
"""

import json
import pathlib
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent


def main() -> int:
    manifest = tomllib.loads((ROOT / "suite" / "manifest.toml").read_text())
    declared = {c["id"]: c for c in manifest["check"]}
    bad, stale, seen = [], [], 0

    for path in sorted((ROOT / "target").glob("suite-*.json")):
        rows = json.loads(path.read_text())
        if not isinstance(rows, list) or not rows:
            continue
        seen += 1
        old_format = [r for r in rows if "measured" not in r]
        if old_format:
            # Not a failure: these were written before the split and the next
            # run of that tier rewrites them. But their `seconds` cannot be
            # told from a timeout ceiling, so say so where someone choosing
            # an optimisation target will see it.
            worst = max(old_format, key=lambda r: r.get("seconds", 0))
            stale.append(f"{path.name}: {len(old_format)} row(s) predate the measured/ceiling "
                         f"split — largest is {worst['id']} at {worst.get('seconds')}s, which "
                         f"may be a timeout kill. Re-run that tier before using it as a target")
            continue
        for row in rows:
            if not row["measured"]:
                continue
            c = declared.get(row["id"])
            if c and row["seconds"] > c["timeout"]:
                bad.append(f"{path.name}: {row['id']} recorded {row['seconds']}s above its own "
                           f"{c['timeout']}s timeout — that is not a completed run")
            if c and c.get("expected") and row["seconds"] > c["expected"] * 8:
                bad.append(f"{path.name}: {row['id']} recorded {row['seconds']}s against a declared "
                           f"{c['expected']}s — 8x out. Re-measure before trusting it as a target")

    if not seen:
        print("check_suite_ledger: no recorded runs yet — nothing to verify")
        return 0
    for line in stale:
        print(f"  STALE FORMAT {line}", file=sys.stderr)
    for line in bad:
        print(f"  {line}", file=sys.stderr)
    print(f"check_suite_ledger: {'FAIL' if bad else 'ok'} — {seen} ledger(s), "
          f"{len(stale)} in the old format")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())

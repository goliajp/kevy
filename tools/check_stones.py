#!/usr/bin/env python3
"""The stone bar, held.

`suite/architecture.toml` names seventeen crates as stone and says any
project could take them. `tools/stone_report.py` measures whether that is
true, four ways. This is the gate that stops it getting worse.

The bar lives in `suite/stone-waivers.toml`, set from the first stone
report rather than from taste, and every stone that misses it today is
named there with the measurement that says so and what would close it.

Two rules make the waiver list a ratchet rather than a wish list:

- **A waiver for a stone that now meets the bar fails.** Passing is how a
  waiver is removed. Otherwise the list rots into a record of problems that
  were fixed years ago, and stops being readable as the list of what is
  still wrong.
- **A stone that misses the bar with no waiver fails.** That is the gate.

Floor rule: a report that covers fewer stones than the architecture map
declares is a broken producer, not a passing gate.

Run: python3 tools/check_stones.py
Exit: 0 pass, 1 violation, 2 refused.
"""

import json
import pathlib
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
REPORT = ROOT / "bench/STONE-REPORT.json"
WAIVERS = ROOT / "suite/stone-waivers.toml"
ARCH = ROOT / "suite/architecture.toml"


def refuse(msg):
    print(f"stonegate: REFUSED — {msg}", file=sys.stderr)
    sys.exit(2)


def check_one(row, bar):
    """-> {rule: why it failed}, for the rules this stone misses."""
    bad = {}
    d = row.get("docs") or {}
    sv = row.get("semver") or {}
    if bar.get("lifts") and not row.get("tested"):
        bad["lifts"] = row.get("lift_note") or "does not lift"
    if row.get("tests", 0) < bar.get("min_tests", 0):
        bad["min_tests"] = f"{row.get('tests', 0)} tests"
    if d.get("pct", 0) < bar.get("doc_pct", 0):
        bad["doc_pct"] = f"{d.get('pct', 0):.0f}% documented"
    if d.get("examples", 0) < bar.get("min_examples", 0):
        bad["min_examples"] = f"{d.get('examples', 0)} executable examples"
    if bar.get("must_be_measured") and not row.get("measured_regions"):
        bad["must_be_measured"] = "absent from the execution corpus"
    if bar.get("semver_clean") and sv and not sv.get("ok"):
        bad["semver_clean"] = sv.get("note", "semver break")
    return bad


def main():
    for p, what in ((REPORT, "stone report"), (WAIVERS, "waiver file"), (ARCH, "architecture map")):
        if not p.exists():
            refuse(f"no {what} at {p.relative_to(ROOT)}")
    doc = json.loads(REPORT.read_text())
    rows = {r["crate"]: r for r in doc["stones"]}
    declared = tomllib.loads(ARCH.read_text())["layers"].get("stone", [])
    if len(rows) < len(declared):
        refuse(f"the report covers {len(rows)} stones, the map declares "
               f"{len(declared)} — the producer failed, this is not a pass")

    w = tomllib.loads(WAIVERS.read_text())
    bar = w["bar"]
    waived = {}
    for e in w.get("waiver", []):
        if not e.get("reason", "").strip() or not e.get("closes_when", "").strip():
            refuse(f"waiver for {e.get('crate')} needs both a reason and closes_when")
        waived.setdefault(e["crate"], set()).update(e["rules"])

    fail, stale = [], []
    for crate in declared:
        row = rows.get(crate)
        if row is None:
            refuse(f"{crate} is declared a stone but absent from the report")
        bad = check_one(row, bar)
        allowed = waived.get(crate, set())
        for rule, why in sorted(bad.items()):
            if rule not in allowed:
                fail.append(f"{crate}: {rule} — {why}")
        for rule in sorted(allowed - set(bad)):
            stale.append(f"{crate}: {rule} — now meets the bar; remove the waiver")

    if doc.get("dead_platform") and doc["dead_platform"] != "linux":
        print(f"stonegate: NOTE — the report's coverage readings come from "
              f"{doc['dead_platform']}, not the enforcing platform")

    if fail or stale:
        print("stonegate: FAIL")
        for f in fail:
            print(f"  ✗ {f}")
        for s in stale:
            print(f"  ⌫ {s}")
        return 1
    n_w = sum(len(v) for v in waived.values())
    print(f"stonegate: PASS — {len(declared)} stones against a "
          f"{len(bar)}-rule bar, {n_w} waived across {len(waived)} crates")
    for c in sorted(waived):
        print(f"    {c}: {', '.join(sorted(waived[c]))}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

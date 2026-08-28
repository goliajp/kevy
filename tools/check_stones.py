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


def pending_publish(row, version, members):
    """Why `lifts` cannot be measured yet, or None.

    `cargo package` strips path dependencies and resolves what is left
    against crates.io. Between a version bump and the publish that follows
    it, every crate with a version-gated sibling is unliftable for that
    reason alone: the version it names is the one this release creates.

    That is a fact about when the measurement was taken, not about the
    crate, and it reads exactly like a real lift failure. It is named here
    instead — loudly, by crate — and it is self-limiting, because after the
    publish the version exists and a crate that still cannot lift fails for
    real on the next run.
    """
    u = row.get("unresolved")
    if not u or row.get("tested"):
        return None
    if u.get("dep") in members and u.get("req") == version:
        return f"{u['dep']} {u['req']} is not on crates.io — this release publishes it"
    return None


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
    if bar.get("semver_clean") and not sv.get("ok"):
        # An ABSENT reading lands here too, and deliberately. `--skip-semver`
        # writes `{}`, and the previous spelling — `and sv and not ok` —
        # made an empty reading satisfy the rule: skipping the check passed
        # it. A producer that did not look is not a stone that is clean.
        bad["semver_clean"] = sv.get("note") or "no semver reading in the report"
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

    version = doc.get("version", "")
    members = set(declared) | {c for c in rows}
    fail, stale, pending = [], [], []
    for crate in declared:
        row = rows.get(crate)
        if row is None:
            refuse(f"{crate} is declared a stone but absent from the report")
        bad = check_one(row, bar)
        allowed = waived.get(crate, set())
        blocked = pending_publish(row, version, members)
        if blocked:
            # `lifts` and everything downstream of it — the crate is never
            # unpacked, so its test count is not a reading either.
            for rule in ("lifts", "min_tests"):
                if rule in bad:
                    pending.append(f"{crate}: {rule} — {blocked}")
                    del bad[rule]
        for rule, why in sorted(bad.items()):
            if rule not in allowed:
                fail.append(f"{crate}: {rule} — {why}")
        for rule in sorted(allowed - set(bad)):
            if blocked and rule in ("lifts", "min_tests"):
                continue  # not stale: it was not measured, so it did not pass
            stale.append(f"{crate}: {rule} — now meets the bar; remove the waiver")

    # A note is not enough. Code switched off by cfg is ABSENT from a
    # coverage run rather than dead in it, so a report taken anywhere but
    # the enforcing platform cannot see kevy-uring at all — the crate does
    # not compile there — and `must_be_measured` then judges a stone the
    # producer never looked at. Two of this file's waivers exist only
    # because a macOS reading was believed. The refusal is what makes the
    # reading's platform part of the verdict instead of a footnote.
    # A report about another release is a different question, exactly as a
    # report from another platform is. The checked-in copy said 5.4.1 while
    # the workspace was 6.0.0, and nothing here noticed.
    want_version = tomllib.loads((ROOT / "Cargo.toml").read_text())[
        "workspace"]["package"]["version"]
    if doc.get("version") != want_version:
        refuse(f"the report is for {doc.get('version') or 'no version'}, and this "
               f"tree is {want_version}. A reading of a different release cannot "
               f"judge this one — take the report on this commit.")

    platform = doc.get("dead_platform")
    if platform != "linux":
        refuse(f"the report's coverage readings come from {platform or 'nowhere'}, "
               f"not the enforcing platform. On any other host the Linux-only "
               f"stones are absent rather than dead, and 'absent' is not a "
               f"score. Take the report on Linux (CI does, in the stonereport "
               f"job) and re-read it here.")

    if fail or stale:
        print("stonegate: FAIL")
        for f in fail:
            print(f"  ✗ {f}")
        for s in stale:
            print(f"  ⌫ {s}")
        return 1
    n_w = sum(len(v) for v in waived.values())
    if pending:
        print(f"stonegate: {len(pending)} reading(s) NOT TAKEN — the tree is "
              f"between a version bump and the publish that creates it:")
        for p_ in pending:
            print(f"    ⧗ {p_}")
        print("    These are not passes. After the publish they resolve, and a "
              "crate that still cannot lift fails here for real.")
    # A major bump lets `cargo semver-checks` skip every lint, because a
    # major is allowed to break anything. `ok` is then true on the strength
    # of nothing having been checked. That is correct of the tool and empty
    # as evidence, so it is named here rather than folded into PASS.
    vacuous = sorted(
        c for c, r in rows.items()
        if (sv := r.get("semver", {})) and sv.get("ok")
        and not sv.get("checks") and sv.get("skipped")
    )
    if vacuous:
        one = rows[vacuous[0]]["semver"]
        print(f"stonegate: {len(vacuous)} semver reading(s) PROVE NOTHING — "
              f"{one.get('skipped')} lints skipped per crate, "
              f"{one.get('baseline') or 'the last release'} → this tree is a "
              f"major bump, and a major may break anything:")
        print(f"    ⊘ {', '.join(vacuous)}")
        print("    Not a compatibility verdict. What a major release can say "
              "instead is what its public surface actually did.")

    tail = f", {n_w} waived across {len(waived)} crates" if n_w else ", none waived"
    if pending:
        tail += f", {len(pending)} reading(s) not taken"
    print(f"stonegate: PASS — {len(declared)} stones against a "
          f"{len(bar)}-rule bar{tail}")
    for c in sorted(waived):
        print(f"    {c}: {', '.join(sorted(waived[c]))}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

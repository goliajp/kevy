#!/usr/bin/env python3
"""One row per stone: can it be lifted, is its API stable, is it documented,
does it run.

`suite/architecture.toml` names seventeen crates as stone — "business-free,
any project could take these, highest quality bar". Until now that was a
list. `check_architecture.py` verified only that their dependencies point
down the layers; nothing measured the bar itself.

Four independent readings per stone, each from a different tool so that no
single failure can make a stone look good:

- **lifts** — `tools/extract_stone.py`: the published form, unpacked
  outside the repository tree and built and tested there.
- **semver** — `cargo semver-checks` against the last published version.
  Not a v6 concern yet, since HEAD and crates.io agree at 5.4.1; it becomes
  one the moment v6 starts changing public API, which is the point of
  recording the clean reading now.
- **docs** — nightly rustdoc `--show-coverage`: documented items, and how
  many carry an **executable** example. Prose is unverified; a doctest is
  compiled and run.
- **dead** — `bench/DEAD-SET.json`: never-executed regions attributed to
  this crate.

This reports. `stonegate` (G3) decides, and its thresholds come from what
this measures rather than from taste — including the one this run made
obvious: a stone can lift, build and pass with zero tests, so a boolean
"tested" proves nothing and the bar needs a count.

Run: python3 tools/stone_report.py [--skip-lift] [--skip-semver]
Exit: 0 wrote the report, 2 refused.
"""

import collections
import glob
import json
import pathlib
import re
import subprocess
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
ARCH = ROOT / "suite/architecture.toml"
DEADSET = ROOT / "bench/DEAD-SET.json"
DOCDIR = ROOT / "target/doc"
OUT_MD = ROOT / "bench/STONE-REPORT.md"
OUT_JSON = ROOT / "bench/STONE-REPORT.json"


def refuse(msg):
    print(f"stonereport: REFUSED — {msg}", file=sys.stderr)
    sys.exit(2)


def stones():
    if not ARCH.exists():
        refuse(f"no {ARCH.relative_to(ROOT)}")
    out = tomllib.loads(ARCH.read_text())["layers"].get("stone", [])
    if not out:
        refuse("the architecture map lists no stones")
    return out


def workspace_version():
    """The release this report is OF — stamped so a reading cannot be used
    to judge a different tree. It is not the semver baseline; that is the
    last published version, which `cargo semver-checks` finds itself."""
    doc = tomllib.loads((ROOT / "Cargo.toml").read_text())
    v = doc.get("workspace", {}).get("package", {}).get("version")
    if not v:
        refuse("no workspace package version to stamp this report with")
    return v


def lifts(skip):
    if skip:
        return {}
    out = ROOT / "target/stone-lift.json"
    subprocess.run([sys.executable, str(ROOT / "tools/extract_stone.py"),
                    "--all", "--json", str(out)], cwd=ROOT,
                   capture_output=True, text=True)
    if not out.exists():
        refuse("extract_stone.py produced no json — exit code would answer "
               "a different question than 'did it measure anything'")
    return {r["crate"]: r for r in json.loads(out.read_text())}


def semver(crate):
    """Compatibility against the last published version — or the absence of one.

    A crate with nothing on the registry has no baseline, and "no baseline"
    is not a failure: there is no promise yet to break. Reporting it as one
    would make every newly published crate look broken on the day it lands,
    which is the day the check matters least. kevy-bench is the first crate
    to hit this, in v6.
    """
    # No `--baseline-version`: the tool picks the last PUBLISHED version,
    # which is the only baseline that means anything. Passing the workspace
    # version instead asks the registry for a release that does not exist
    # yet, and every crate then came back "unpublished, 0 checks, ok" —
    # eighteen stones reported clean on the strength of checking nothing.
    p = subprocess.run(["cargo", "semver-checks", "check-release", "-p", crate],
                       cwd=ROOT, capture_output=True, text=True, timeout=900)
    log = p.stdout + p.stderr
    if re.search(r"no crate named|not found in registry|failed to select a version for the requirement `"
                 + re.escape(crate), log) or "no published version" in log:
        return {"ok": True, "checks": 0, "skipped": 0, "unpublished": True,
                "baseline": "", "note": "not on crates.io yet — no baseline to break"}
    m = re.search(r"(\d+) checks: (\d+) pass(?:, (\d+) skip)?", log)
    b = re.search(r"Checking \S+ v(\S+) -> v\S+ \((\w+) change\)", log)
    checks = int(m.group(1)) if m else 0
    skipped = int(m.group(3)) if m and m.group(3) else 0
    row = {"ok": p.returncode == 0, "checks": checks, "skipped": skipped,
           "unpublished": False, "baseline": b.group(1) if b else "",
           "note": "" if p.returncode == 0 else
                   next((l.strip() for l in log.splitlines() if "--- failure" in l), "failed")[:120]}
    # A major bump lets the tool skip every lint, because a major is allowed
    # to break anything. That is correct of the tool and vacuous as evidence:
    # say so in the row rather than let `ok` carry a meaning it does not have.
    if p.returncode == 0 and checks == 0 and skipped:
        row["note"] = (f"{skipped} lints skipped — {b.group(2) if b else 'major'} bump "
                       f"from {row['baseline'] or 'the last release'}; a major may break "
                       "anything, so this reading proves nothing")
    return row


def docs():
    """documented / total / with-example, per crate, from rustdoc coverage."""
    out = {}
    for f in glob.glob(str(DOCDIR / "*.txt")):
        m = re.search(r"\|\s*Total\s*\|\s*(\d+)\s*\|\s*([\d.]+)%\s*\|\s*(\d+)\s*\|",
                      pathlib.Path(f).read_text())
        if not m:
            continue
        d, pct, ex = int(m.group(1)), float(m.group(2)), int(m.group(3))
        out[pathlib.Path(f).stem.replace("_", "-")] = {
            "documented": d, "items": round(d / pct * 100) if pct else 0,
            "pct": pct, "examples": ex,
        }
    return out


def dead_by_crate():
    """-> ({crate: {regions, dead}}, platform).

    Carries the denominator on purpose. A crate the corpus never compiled
    reports zero dead regions and, without `regions`, outranks every crate
    that was actually measured. That is not a good score, it is no score.
    """
    if not DEADSET.exists():
        return {}, None
    doc = json.loads(DEADSET.read_text())
    return doc.get("crates", {}), doc.get("platform")


def main():
    args = sys.argv[1:]
    version = workspace_version()
    lift = lifts("--skip-lift" in args)
    dc = docs()
    dead, dead_platform = dead_by_crate()
    if not dc:
        refuse("no rustdoc coverage tables in target/doc; run "
               "`RUSTDOCFLAGS='-Z unstable-options --show-coverage' "
               "cargo +nightly doc --workspace --no-deps` first")

    rows = []
    for c in stones():
        cov = dead.get(c) or {}
        row = {"crate": c, "dead_regions": cov.get("dead", 0),
               "measured_regions": cov.get("regions", 0)}
        row.update({k: lift.get(c, {}).get(k) for k in ("packaged", "built", "tested", "tests")})
        row["lift_note"] = lift.get(c, {}).get("note", "")
        # Carried so the gate can tell "this crate does not lift" from "the
        # version its sibling is pinned to does not exist yet, because this
        # is the release that publishes it".
        row["unresolved"] = lift.get(c, {}).get("unresolved")
        row["docs"] = dc.get(c, {})
        row["semver"] = {} if "--skip-semver" in args else semver(c)
        rows.append(row)
        d = row["docs"]
        print(f"  {c:<16} lift={row['tested']} tests={row['tests']} "
              f"doc={d.get('pct', 0):.0f}% ex={d.get('examples', 0)} "
              f"dead={row['dead_regions'] if row['measured_regions'] else 'NOT MEASURED'}")

    OUT_JSON.write_text(json.dumps(
        {"version": version, "dead_platform": dead_platform, "stones": rows},
        indent=2) + "\n")
    write_md(rows, version, dead_platform)
    print(f"stonereport: {len(rows)} stones -> {OUT_MD.relative_to(ROOT)}")
    return 0


def write_md(rows, version, dead_platform):
    lifted = sum(1 for r in rows if r["tested"])
    notests = [r["crate"] for r in rows if r["tested"] and not r["tests"]]
    unmeasured = [r["crate"] for r in rows if not r["measured_regions"]]
    noex = [r["crate"] for r in rows if not r["docs"].get("examples")]
    out = [
        "# Stone report", "",
        f"Seventeen crates classified `stone` in `suite/architecture.toml`, "
        f"at workspace version {version}.", "",
        "Generated by `tools/stone_report.py`; do not edit. Four readings per",
        "stone, each from a different tool, so no single failure can make a",
        "stone look good.", "",
        f"**{lifted}/{len(rows)} lift** — unpack the published form outside the",
        "repository and its tests pass there.", "",
        f"**{len(noex)}/{len(rows)} carry no executable example.** Documentation",
        "that is not compiled is a promise nobody checked.", "",
    ]
    if notests:
        out += [f"**{', '.join(notests)} lift with zero tests** — which is why a",
                "boolean `tested` cannot be the bar.", ""]
    if unmeasured:
        out += [f"**{', '.join(unmeasured)} is not in the corpus at all** — zero",
                "regions compiled, so its zero dead regions are no score rather",
                "than a good one.", ""]
    if dead_platform and dead_platform != "linux":
        out += [f"Dead-region counts are from a **{dead_platform}** run and are",
                "not baseline material: cfg(linux) code is absent from that run",
                "rather than dead in it.", ""]
    out += ["| stone | lifts | tests | docs | examples | dead regions | semver |",
            "|---|---|---:|---:|---:|---:|---|"]
    for r in sorted(rows, key=lambda r: (r["tested"] is not True, -r["dead_regions"])):
        d, s = r["docs"], r["semver"]
        lift = "yes" if r["tested"] else ("builds" if r["built"] else "no")
        sv = ("—" if not s else "unpublished" if s.get("unpublished")
              else "clean" if s["ok"] else f"**{s['note']}**")
        note = f" — {r['lift_note']}" if r["lift_note"] else ""
        dr = (f"{r['dead_regions']}/{r['measured_regions']}"
              if r["measured_regions"] else "**not measured**")
        out.append(f"| {r['crate']}{note} | {lift} | {r['tests']} | "
                   f"{d.get('documented', 0)}/{d.get('items', 0)} "
                   f"({d.get('pct', 0):.0f}%) | {d.get('examples', 0)} | "
                   f"{dr} | {sv} |")
    OUT_MD.write_text("\n".join(out) + "\n")


if __name__ == "__main__":
    sys.exit(main())

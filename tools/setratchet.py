#!/usr/bin/env python3
"""A ratchet over a set of identities, not over a number.

Every baseline this repository had before v6 stored a scalar with a
tolerance band: a coverage percentage, a throughput figure, bytes per
entry. That is the right instrument for a quantity and the wrong one for a
set. `COV-BASELINE.json` can hold 79.64% for a year while the identity of
the uncovered fifth is completely replaced — the number never moves, so
the gate never fires, and nothing was actually held.

A set-ratchet holds identities:

    {"kevy_geo::estimate_step": 1}

— one never-executed region inside that function. The rule is that **no
symbol may gain, and no symbol may join**. Shrinking and disappearing are
what progress looks like and are always allowed.

Three decisions worth stating, because each has an obvious wrong answer:

**Identity is the symbol, not the line.** Line numbers churn on every edit
above them, and a baseline that churns is a baseline nobody reads. A symbol
moves only when it is renamed, which is a change its author should be
declaring anyway.

**Growth is possible but never silent.** Some growth is legitimate — new
platform-gated code, a new panic edge. `--accept-growth` takes a reason and
writes it into the baseline's own history, so a loosening leaves a record
where the next reader will find it. `--update` alone refuses to record a
worse set; a ratchet that can be quietly reset is a ratchet in name only.

**Identity mismatch refuses rather than fails.** A dead set measured under
a different corpus or on a different platform is not a worse set, it is a
different question. Reporting that difference as a regression would be the
device answering something nobody asked.

Run:
  python3 tools/setratchet.py gate    <baseline.json> <observed.json>
  python3 tools/setratchet.py update  <baseline.json> <observed.json>
              [--accept-growth "reason"]
Exit: 0 pass/updated, 1 regression, 2 refused.
"""

import json
import pathlib
import sys

# An observed set this small means the producer failed, not that the code
# became perfect. Callers pass their own floor; this is the backstop.
DEFAULT_MIN_TOTAL = 1


def refuse(msg):
    print(f"setratchet: REFUSED — {msg}", file=sys.stderr)
    sys.exit(2)


def load(path, what):
    p = pathlib.Path(path)
    if not p.exists():
        refuse(f"no {what} at {path}")
    try:
        doc = json.loads(p.read_text())
    except json.JSONDecodeError as e:
        refuse(f"{what} at {path} is not JSON: {e}")
    if "symbols" not in doc or not isinstance(doc["symbols"], dict):
        refuse(f"{what} at {path} has no `symbols` map")
    return doc


def identity(doc):
    """What makes two measurements comparable at all."""
    ident = doc.get("identity")
    if ident is None:
        ident = {k: doc[k] for k in ("corpus", "platform", "kind") if k in doc}
    if not ident:
        refuse("a set with no identity cannot be compared; "
               "record at least corpus and platform")
    return ident


def compare(base, obs):
    """-> (grew, joined, shrank, left), with declared-unstable symbols held out.

    A symbol whose count is not reproducible cannot be ratcheted: it would
    fail on its own noise and teach everyone to re-run until green, which
    is worse than no gate. The exemption comes from the BASELINE rather
    than from the observed set, so the tolerance is part of what was
    recorded and cannot be widened by the run being judged.
    """
    unstable = set(base.get("unstable", []))
    b = {k: v for k, v in base["symbols"].items() if k not in unstable}
    o = {k: v for k, v in obs["symbols"].items() if k not in unstable}
    grew = {k: (b[k], o[k]) for k in b.keys() & o.keys() if o[k] > b[k]}
    joined = {k: o[k] for k in o.keys() - b.keys()}
    shrank = {k: (b[k], o[k]) for k in b.keys() & o.keys() if o[k] < b[k]}
    left = {k: b[k] for k in b.keys() - o.keys()}
    return grew, joined, shrank, left


def report(grew, joined, shrank, left, base, obs):
    bt, ot = sum(base["symbols"].values()), sum(obs["symbols"].values())
    print(f"  baseline {len(base['symbols'])} symbols / {bt} regions"
          f"   observed {len(obs['symbols'])} / {ot}")
    for k, n in sorted(joined.items())[:20]:
        print(f"  + JOINED  {k}  ({n})")
    if len(joined) > 20:
        print(f"  … and {len(joined) - 20} more joined")
    for k, (a, c) in sorted(grew.items())[:20]:
        print(f"  ^ GREW    {k}  {a} -> {c}")
    if len(grew) > 20:
        print(f"  … and {len(grew) - 20} more grew")
    if shrank or left:
        print(f"  (improved: {len(shrank)} shrank, {len(left)} left the set)")


def gate(base_path, obs_path):
    base, obs = load(base_path, "baseline"), load(obs_path, "observed")
    bi, oi = identity(base), identity(obs)
    if bi != oi:
        refuse(f"identity mismatch: baseline {bi} vs observed {oi} — "
               f"these answer different questions, not better and worse")
    total = sum(obs["symbols"].values())
    floor = base.get("min_total", DEFAULT_MIN_TOTAL)
    if obs.get("total_regions", floor) < floor:
        refuse(f"observed total {obs.get('total_regions')} is below the floor "
               f"{floor}; the producer failed rather than the code improving")

    grew, joined, shrank, left = compare(base, obs)
    if grew or joined:
        print(f"setratchet: FAIL — the set grew ({len(joined)} joined, {len(grew)} grew)")
        report(grew, joined, shrank, left, base, obs)
        print("  To accept deliberately: setratchet.py update ... "
              "--accept-growth \"why this is correct\"")
        return 1
    n_unstable = len(base.get("unstable", []))
    tail = f", {n_unstable} declared unstable and not held" if n_unstable else ""
    print(f"setratchet: PASS — {len(base['symbols']) - n_unstable} symbols held, "
          f"{len(shrank)} shrank, {len(left)} left ({total} regions){tail}")
    return 0


def update(base_path, obs_path, reason):
    obs = load(obs_path, "observed")
    p = pathlib.Path(base_path)
    history = []
    if p.exists():
        base = load(base_path, "baseline")
        if identity(base) != identity(obs):
            refuse(f"identity mismatch: {identity(base)} vs {identity(obs)}; "
                   f"a corpus change needs a new baseline, not an update")
        history = base.get("history", [])
        grew, joined, _, _ = compare(base, obs)
        if (grew or joined) and not reason:
            print("setratchet: FAIL — refusing to record a worse set without a reason")
            report(grew, joined, {}, {}, base, obs)
            return 1
        if grew or joined:
            history = history + [{
                "accepted": sorted(joined) + sorted(grew),
                "reason": reason,
                "delta": sum(joined.values()) + sum(c - a for a, c in grew.values()),
            }]
    out = dict(obs)
    out["history"] = history
    p.write_text(json.dumps(out, indent=2, sort_keys=False) + "\n")
    print(f"setratchet: recorded {len(obs['symbols'])} symbols "
          f"/ {sum(obs['symbols'].values())} regions -> {base_path}")
    if reason:
        print(f"  growth accepted: {reason}")
    return 0


def main():
    args = sys.argv[1:]
    if len(args) < 3 or args[0] not in ("gate", "update"):
        refuse("usage: setratchet.py gate|update <baseline.json> <observed.json> "
               "[--accept-growth \"reason\"]")
    mode, base_path, obs_path = args[0], args[1], args[2]
    reason = ""
    if "--accept-growth" in args:
        i = args.index("--accept-growth")
        if i + 1 >= len(args):
            refuse("--accept-growth needs a reason")
        reason = args[i + 1]
    return gate(base_path, obs_path) if mode == "gate" else update(base_path, obs_path, reason)


if __name__ == "__main__":
    sys.exit(main())

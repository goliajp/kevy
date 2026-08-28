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
    spec = base.get("unstable") or {}
    if isinstance(spec, list):        # the first baseline shape: names only
        spec = {"symbols": spec, "prefixes": []}
    names = set(spec.get("symbols", []))
    prefixes = tuple(spec.get("prefixes", []))

    def held(k):
        return k not in names and not (prefixes and k.startswith(prefixes))

    b = {k: v for k, v in base["symbols"].items() if held(k)}
    o = {k: v for k, v in obs["symbols"].items() if held(k)}
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
    spec = base.get("unstable") or {}
    if isinstance(spec, list):
        spec = {"symbols": spec, "prefixes": []}
    names = set(spec.get("symbols", []))
    prefixes = tuple(spec.get("prefixes", []))
    exempt = sum(
        1 for k in base["symbols"]
        if k in names or (prefixes and k.startswith(prefixes))
    )
    tail = (f", {exempt} exempt under {len(names)} symbol / "
            f"{len(prefixes)} prefix declarations" if exempt else "")
    print(f"setratchet: PASS — {len(base['symbols']) - exempt} symbols held, "
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


def envelope(base_path, obs_paths, reason=""):
    """Record the element-wise MAXIMUM across several observed sets.

    A ratchet over a measurement with a noisy tail cannot hold one sample:
    three consecutive CI runs of this workspace each produced a different
    handful of grown symbols, none overlapping — match_migrating and
    resolve_xadd_id in one, maybe_ack and dispatch_with_proto and os::trim
    in the next. Registering them as they appear does not converge, because
    the next run names different ones.

    So the baseline holds the UPPER BOUND of what the noise has been
    observed to do. Growth then means "worse than the worst of N runs",
    which is a claim about the code rather than about which run it was.
    The count of runs is recorded, because an envelope over one run is
    just a sample and should not be able to pass itself off as more.
    """
    docs = [load(p, f"observed {i + 1}") for i, p in enumerate(obs_paths)]
    ids = {json.dumps(identity(d), sort_keys=True) for d in docs}
    if len(ids) != 1:
        refuse(f"the {len(docs)} observed sets do not share one identity: {ids}")

    merged = {}
    for d in docs:
        for k, v in d["symbols"].items():
            merged[k] = max(merged.get(k, 0), v)

    # The same rule `update` has, for the same reason. This function wrote
    # the element-wise maximum with no reference to the prior baseline, so
    # it would absorb genuine new dead code as readily as a re-recording —
    # which makes it the quiet reset this file's own docstring says a
    # ratchet must not have. Growth against the prior baseline needs a
    # reason here too; a fall does not.
    prior_path = pathlib.Path(base_path)
    if prior_path.exists():
        prior = load(base_path, "baseline")
        if identity(prior) == identity(docs[-1]):
            synthetic = dict(docs[-1])
            synthetic["symbols"] = merged
            grew, joined, _, _ = compare(prior, synthetic)
            if (grew or joined) and not reason:
                print("setratchet: FAIL — the envelope is worse than the baseline "
                      "and no reason was given")
                report(grew, joined, {}, {}, prior, synthetic)
                return 1

    out = dict(docs[-1])
    out["symbols"] = dict(sorted(merged.items()))
    out["envelope_runs"] = len(docs)
    out["dead_regions"] = sum(merged.values())
    out["history"] = (load(base_path, "baseline").get("history", []) if prior_path.exists() else []) + [
        {"envelope_over": len(docs),
         "symbols": len(merged),
         "regions": out["dead_regions"],
         "reason": reason or "element-wise maximum; growth now means worse than the worst observed run"},
    ]
    prior_path.write_text(json.dumps(out, indent=2) + "\n")
    per = [d["dead_regions"] for d in docs]
    print(f"setratchet: envelope over {len(docs)} runs -> {len(merged)} symbols / "
          f"{out['dead_regions']} regions (individual runs: {per})")
    return 0


def main():
    args = sys.argv[1:]
    if len(args) < 3 or args[0] not in ("gate", "update", "envelope"):
        refuse("usage: setratchet.py gate|update <baseline.json> <observed.json> "
               "[--accept-growth \"reason\"]\n"
               "       setratchet.py envelope <baseline.json> <observed.json>...")
    reason = ""
    if "--accept-growth" in args:
        i = args.index("--accept-growth")
        if i + 1 >= len(args):
            refuse("--accept-growth needs a reason")
        reason = args[i + 1]
        args = args[:i] + args[i + 2:]
    if args[0] == "envelope":
        return envelope(args[1], args[2:], reason)
    mode, base_path, obs_path = args[0], args[1], args[2]
    return gate(base_path, obs_path) if mode == "gate" else update(base_path, obs_path, reason)


if __name__ == "__main__":
    sys.exit(main())

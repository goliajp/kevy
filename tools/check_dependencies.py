#!/usr/bin/env python3
"""Every dependency outside the kevy-* family is declared, with a reason.

The L2 constraint — pure Rust, no crates.io dependencies, the one libc
boundary hand-bound in kevy-sys — has been prose since it was set. Prose
does not stop a dependency from arriving. This is the mechanical form,
built the same way `check_architecture.py` (5.3) made the layering model
mechanical.

What it enforces:

- **Set equality, both directions.** An undeclared non-kevy dependency
  fails; a declared one that no longer exists fails too. The map must not
  rot, which is the failure mode a one-directional allowlist invites.
- **`ships` is recomputed, never trusted.** A dependency ships when its
  kind is normal, its target cfg is unconditional, and it is either
  non-optional or reachable from the default feature closure. The
  declaration states a claim; disagreement with the computed value is a
  failure. Otherwise the interesting field would be settable by whoever
  wanted the gate quiet.
- **No shipping third-party dependency in a stone.** A stone is liftable
  into any project, so a third-party dependency inside one is a dependency
  it hands to everyone who lifts it. Layers come from
  `suite/architecture.toml`, and the two maps are cross-checked against
  each other — a declaration naming the wrong layer fails.

Floor rule: finding no packages, or no declarations, is a failure of the
selector, not a pass.

Run: python3 tools/check_dependencies.py
Exit: 0 pass, 1 violation, 2 refused (cannot see the subject).
"""

import json
import pathlib
import subprocess
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
DECL = ROOT / "suite/dependencies.toml"
ARCH = ROOT / "suite/architecture.toml"
FAMILY = "kevy"

# Below this, the selector is broken rather than the tree empty.
MIN_PACKAGES = 40


def refuse(msg):
    print(f"depgate: REFUSED — {msg}", file=sys.stderr)
    sys.exit(2)


def load_layers():
    """crate -> layer, from the architecture map."""
    if not ARCH.exists():
        refuse(f"no {ARCH.relative_to(ROOT)} to read layers from")
    layers = tomllib.loads(ARCH.read_text())["layers"]
    out = {}
    for layer, crates in layers.items():
        for c in crates:
            out[c] = layer
    if not out:
        refuse("the architecture map classified no crates")
    return out


def metadata():
    try:
        raw = subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--no-deps"],
            cwd=ROOT, capture_output=True, text=True, check=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as e:
        refuse(f"cargo metadata failed: {e}")
    pkgs = json.loads(raw)["packages"]
    if len(pkgs) < MIN_PACKAGES:
        refuse(f"cargo metadata returned {len(pkgs)} packages; this is not the workspace")
    return pkgs


def default_closure(features):
    """Feature names and `dep:` activations reachable from `default`."""
    seen, stack = set(), list(features.get("default", []))
    while stack:
        f = stack.pop()
        if f in seen:
            continue
        seen.add(f)
        stack.extend(features.get(f, []))
        if "/" in f and not f.startswith("dep:"):
            seen.add(f.split("/", 1)[0])
    return seen


def observed(pkgs):
    """(crate, dep, target) -> {kinds, optional, in_default, ships}."""
    out = {}
    for p in pkgs:
        closure = default_closure(p.get("features", {}))
        for d in p["dependencies"]:
            if d["name"].startswith(FAMILY):
                continue
            key = (p["name"], d["name"], d.get("target") or "-")
            rec = out.setdefault(key, {"kinds": set(), "optional": False,
                                       "in_default": False, "ships": False})
            kind = d["kind"] or "normal"
            rec["kinds"].add(kind)
            if kind != "normal":
                continue
            opt = bool(d["optional"])
            in_def = (not opt) or (d["name"] in closure or f"dep:{d['name']}" in closure)
            rec["optional"] |= opt
            rec["in_default"] |= in_def
            # Ships only when unconditional and reachable in a default build.
            rec["ships"] |= (d.get("target") is None) and in_def
    return out


def declared():
    if not DECL.exists():
        refuse(f"no {DECL.relative_to(ROOT)} to read declarations from")
    doc = tomllib.loads(DECL.read_text())
    entries = doc.get("dependency", [])
    if not entries:
        refuse("the declaration file lists no dependencies")
    out = {}
    for e in entries:
        key = (e["crate"], e["dep"], e.get("target", "-"))
        if key in out:
            refuse(f"duplicate declaration for {key}")
        if not e.get("reason", "").strip():
            refuse(f"declaration for {key} carries no reason")
        out[key] = e
    return out, doc.get("rules", {})


def fmt(key):
    c, d, t = key
    return f"{c} -> {d}" + ("" if t == "-" else f" [target {t}]")


def main():
    layers = load_layers()
    obs = observed(metadata())
    dec, rules = declared()
    fail = []

    for key in sorted(obs.keys() - dec.keys()):
        fail.append(f"UNDECLARED dependency {fmt(key)} — add it to suite/dependencies.toml with a reason")
    for key in sorted(dec.keys() - obs.keys()):
        fail.append(f"STALE declaration {fmt(key)} — the dependency is gone; remove the entry")

    for key in sorted(obs.keys() & dec.keys()):
        o, d = obs[key], dec[key]
        if set(d.get("kinds", [])) != o["kinds"]:
            fail.append(f"KIND MISMATCH {fmt(key)}: declared {sorted(d.get('kinds', []))}, actual {sorted(o['kinds'])}")
        for field in ("optional", "in_default", "ships"):
            if bool(d.get(field)) != o[field]:
                fail.append(f"{field.upper()} MISMATCH {fmt(key)}: declared {d.get(field)}, computed {o[field]}")
        actual_layer = layers.get(key[0])
        if actual_layer is None:
            fail.append(f"UNCLASSIFIED crate {key[0]} — not in suite/architecture.toml")
        elif d.get("layer") != actual_layer:
            fail.append(f"LAYER MISMATCH {fmt(key)}: declared {d.get('layer')}, architecture map says {actual_layer}")

        if o["ships"] and d.get("provenance") == "third-party":
            if actual_layer in rules.get("no_shipping_third_party_in", []):
                fail.append(f"RULE {fmt(key)}: a shipping third-party dependency in a {actual_layer} crate")

    if fail:
        print("depgate: FAIL")
        for f in fail:
            print(f"  {f}")
        return 1

    ships = sorted(k for k, v in obs.items() if v["ships"])
    third = [k for k in ships if dec[k].get("provenance") == "third-party"]
    print(f"depgate: PASS — {len(obs)} non-{FAMILY} dependencies across "
          f"{len({k[0] for k in obs})} crates, all declared")
    print(f"  ships in a default build: {len(ships)} "
          f"({len(third)} third-party, {len(ships) - len(third)} first-party)")
    for k in ships:
        print(f"    {fmt(k)} [{dec[k].get('provenance')}, {dec[k].get('layer')}]")
    return 0


if __name__ == "__main__":
    sys.exit(main())

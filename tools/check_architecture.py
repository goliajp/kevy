#!/usr/bin/env python3
"""Crate layering follows the stone/steel/cement model, mechanically.

The model (methodology, long-standing): stone is business-free and may
depend only on stone; steel knows the domain and may depend on steel and
stone; cement is a product face and may depend on anything; support is
dev-only scaffolding, exempt from the direction rule.

Until 5.3 the model lived in prose. This makes it a gate:

- **Every workspace crate must be classified** in
  suite/architecture.toml. An unclassified crate fails — a new crate
  cannot slip in below the model. (A classified crate that no longer
  exists also fails: the map must not rot.)
- **Dependency direction** is checked over `cargo metadata`'s *normal*
  dependencies only. Dev and build dependencies do not ship, and the
  workspace legitimately uses them upward (a stone crate's tests may use
  kevy-testnet).
- The floor rule applies to the gate itself: finding zero crates or an
  empty layer map is a failure of the selector, not a pass.

Run: python3 tools/check_architecture.py
"""

import json
import pathlib
import subprocess
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
MAP = ROOT / "suite/architecture.toml"

# Which layers a layer may depend on (normal deps).
ALLOWED = {
    "stone": {"stone"},
    "steel": {"steel", "stone"},
    "cement": {"cement", "steel", "stone"},
    "support": {"support", "cement", "steel", "stone"},
}


def main():
    layers = tomllib.loads(MAP.read_text(encoding="utf-8"))["layers"]
    layer_of = {}
    for layer, crates in layers.items():
        for c in crates:
            if c in layer_of:
                print(f"check_architecture: {c} classified twice ({layer_of[c]} and {layer})")
                return 1
            layer_of[c] = layer
    if len(layer_of) < 30:
        print(f"check_architecture: only {len(layer_of)} crates in the map — the map is broken")
        return 1

    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--no-deps"],
            capture_output=True, text=True, cwd=ROOT, check=True,
        ).stdout
    )
    packages = {p["name"]: p for p in meta["packages"]}
    if len(packages) < 30:
        print(f"check_architecture: cargo metadata found only {len(packages)} crates")
        return 1

    bad = []
    unclassified = sorted(set(packages) - set(layer_of))
    for c in unclassified:
        bad.append(f"{c} is not classified in suite/architecture.toml — every crate carries a layer")
    for c in sorted(set(layer_of) - set(packages)):
        bad.append(f"{c} is classified but no longer in the workspace — the map rotted")

    for name, p in sorted(packages.items()):
        layer = layer_of.get(name)
        if layer is None:
            continue
        for dep in p["dependencies"]:
            # Normal deps only: dev/build deps do not ship.
            if dep.get("kind") is not None or not dep["name"].startswith("kevy"):
                continue
            dep_layer = layer_of.get(dep["name"])
            if dep_layer is None:
                continue  # already reported as unclassified
            if dep_layer not in ALLOWED[layer]:
                bad.append(
                    f"{name} ({layer}) depends on {dep['name']} ({dep_layer}) — "
                    f"{layer} may reach only {{{', '.join(sorted(ALLOWED[layer]))}}}"
                )

    if bad:
        print(f"check_architecture: FAIL — {len(bad)} problem(s)")
        for b in bad:
            print(f"  ✗ {b}")
        return 1

    edges = sum(
        1
        for p in packages.values()
        for d in p["dependencies"]
        if d.get("kind") is None and d["name"].startswith("kevy")
    )
    print(
        f"ok: {len(packages)} crates classified "
        f"(stone {len(layers['stone'])}, steel {len(layers['steel'])}, "
        f"cement {len(layers['cement'])}, support {len(layers['support'])}), "
        f"{edges} shipping edges all point downward"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())

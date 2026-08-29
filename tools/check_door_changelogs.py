#!/usr/bin/env python3
"""A door's changelog must mention the version the door is shipping.

The seven-layer version check proves every manifest says the same
number. Nothing proved that the CHANGELOG beside a manifest mentions
it — and `dart pub publish` warns about exactly that, in a dry run
nobody was reading:

    ./CHANGELOG.md doesn't mention current version (6.0.0).
    Package has 1 warning.

flutter_kevy's changelog stopped at 5.3.0 and two releases went past it
with the version bumped. pub.dev shows that file to anyone deciding
whether to depend on the package, so a stale one is not cosmetic: it
says the door has not moved since 5.3.

Run: python3 tools/check_door_changelogs.py
Exit: 0 agree, 1 a changelog is behind, 2 refused (the read is broken).
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

# (changelog, manifest, how the manifest states its version)
DOORS = [
    ("bindings/flutter/CHANGELOG.md", "bindings/flutter/pubspec.yaml",
     re.compile(r"^version:\s*([0-9]+\.[0-9]+\.[0-9]+)", re.M)),
]


def main() -> int:
    if not DOORS:
        print("check_door_changelogs: REFUSED — no doors declared", file=sys.stderr)
        return 2

    behind = []
    for changelog, manifest, pattern in DOORS:
        cl, mf = ROOT / changelog, ROOT / manifest
        if not cl.exists() or not mf.exists():
            print(f"check_door_changelogs: REFUSED — {changelog} or {manifest} is missing",
                  file=sys.stderr)
            return 2
        m = pattern.search(mf.read_text())
        if not m:
            print(f"check_door_changelogs: REFUSED — no version found in {manifest}; "
                  "the field moved and this check went blind", file=sys.stderr)
            return 2
        version = m.group(1)
        if version not in cl.read_text():
            behind.append((changelog, manifest, version))

    if behind:
        print("check_door_changelogs: FAIL — a door ships a version its changelog "
              "does not mention")
        for changelog, manifest, version in behind:
            print(f"  {manifest} says {version}; {changelog} never says it")
        print("  The registry shows that file to whoever is deciding to depend on it.")
        return 1

    print(f"check_door_changelogs: ok — {len(DOORS)} door(s), each changelog names "
          "the version its manifest ships")
    return 0


if __name__ == "__main__":
    sys.exit(main())

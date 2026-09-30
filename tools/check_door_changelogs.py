#!/usr/bin/env python3
"""Every door's changelog must have an entry for the version the door ships.

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

That check covered one door, from a list written here. Until 7.0 no other
door had a changelog at all, so npm, PyPI, NuGet and Maven showed a
version number and nothing about what it changed — 7.0 moved the Go
module path and changed an error text in six bindings, and a reader of
those registry pages had no way to find out. So the door list is now
DERIVED, the same way tools/check_channels_published.py derives it: a
manifest under bindings/, packaging/ or crates/ that names a package is a
door, and so is every door that script records as shipping outside a
registry (SwiftPM and CMake resolve the tag; the Android door ships inside
expo-kevy; the Tauri plugin is taken by path). A new door is checked the
day it exists.

The entry has to be a heading, `## <version>`. A substring match would
pass a changelog whose only mention of the version is in the prose of an
older entry ("since 7.0.0"), which says nothing about what 7.0.0 did.

Run: python3 tools/check_door_changelogs.py
Exit: 0 agree, 1 a changelog is missing or behind, 2 refused (the read is broken).
"""

import pathlib
import re
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import check_channels_published as channels  # noqa: E402

ROOT = channels.ROOT

# Fewer doors than this means the derivation broke, not that doors went
# away: 13 registry doors and the 4 that ship from the tag when this was
# written. A smaller gate that passes is worse than no gate.
FLOOR = 17

# Where the changelog of each door check_channels_published.py lists as
# NOT_PUBLISHED lives. Every such door must appear here or in EXEMPT, so
# adding one to that list forces a decision about its changelog.
UNREGISTERED = {
    "bindings/apple": "bindings/apple/KevyKit",
    "bindings/cpp": "bindings/cpp",
    "bindings/android": "bindings/android",
    "bindings/tauri/tauri-plugin-kevy": "bindings/tauri/tauri-plugin-kevy",
}

# Door directories that legitimately carry no changelog, each with the
# reason. Empty: every door has one.
EXEMPT: dict[str, str] = {}


def refuse(msg: str) -> int:
    print(f"check_door_changelogs: REFUSED — {msg}", file=sys.stderr)
    return 2


def doors():
    """(door directory, version it ships, where the version came from)."""
    excused = {ROOT / k for k in channels.NOT_PUBLISHED}
    tag_version = channels.workspace_version()
    out = []
    for _, _, _, manifest, declared in channels.binding_doors(excused):
        # a Go module carries no version of its own; the tag is its version
        out.append((manifest.parent, declared or tag_version,
                    manifest.relative_to(ROOT) if declared else "Cargo.toml"))
    for key in channels.NOT_PUBLISHED:
        if key in EXEMPT:
            continue
        out.append((ROOT / UNREGISTERED[key], tag_version, "Cargo.toml"))
    return out


def main() -> int:
    unmapped = sorted(set(channels.NOT_PUBLISHED) - set(UNREGISTERED) - set(EXEMPT))
    if unmapped:
        return refuse(f"no changelog location or exemption for {', '.join(unmapped)}")

    found = doors()
    if len(found) < FLOOR:
        return refuse(f"found {len(found)} doors, expected at least {FLOOR}; "
                      "the derivation is broken")

    missing, behind, checked = [], [], 0
    for door, version, source in found:
        rel = door.relative_to(ROOT)
        if str(rel) in EXEMPT:
            continue
        checked += 1
        cl = door / "CHANGELOG.md"
        if not cl.exists():
            missing.append(f"{rel}/CHANGELOG.md does not exist ({source} says {version})")
            continue
        heading = re.compile(rf"^##\s+v?{re.escape(version)}(?![\w.-])", re.M)
        if not heading.search(cl.read_text(encoding="utf-8")):
            behind.append(f"{rel}/CHANGELOG.md has no `## {version}` heading "
                          f"({source} says {version})")

    if missing or behind:
        print("check_door_changelogs: FAIL — a door ships a version its changelog "
              "does not describe")
        for line in missing + behind:
            print(f"  {line}")
        print("  The registry shows that file to whoever is deciding to depend on it.")
        return 1

    print(f"check_door_changelogs: ok — {checked} door(s), each changelog has an "
          f"entry for the version its manifest ships"
          + (f"; exempt: {', '.join(sorted(EXEMPT))}" if EXEMPT else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())

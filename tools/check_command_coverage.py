#!/usr/bin/env python3
"""What the current Redis serves that kevy does not.

The anchors gate keeps the engines we MEASURE against current. This one
keeps the surface we're measured against honest: Redis 8.8 added a whole
native data type (18 AR* commands), 8.10 added LMOVEM, SUNIONCARD and
more, and nothing in this repository would have said so. A comparison
that runs against a current Redis while implementing a 2024 command set
is answering an easier question than the one it appears to answer.

So: ask the pinned redis image for its own COMMAND LIST, read kevy's
VERB_META (the single source of truth behind COMMAND, llms.txt and the
MCP schema), and classify every verb Redis has and kevy does not:

  exempt        — a decision already made and written down (AUTH, cluster,
                  the absorbed modules). Reported as a count, not as work.
  planned       — a gap with an RFC that owns it. Still a gap.
  UNCLASSIFIED  — neither. This FAILS, because a new Redis release should
                  arrive as a decision to make and not as silence.

Run: python3 tools/check_command_coverage.py [--refresh] [--list]
     --refresh  re-asks the pinned redis image and rewrites the captured set
"""

import json
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
COVERAGE = ROOT / "bench" / "COMMAND-COVERAGE.json"
ANCHORS = ROOT / "bench" / "COMPETITOR-ANCHORS.json"
VERB_META = ROOT / "crates" / "kevy" / "src" / "verb_meta"


def kevy_verbs() -> set:
    """Every dispatch-reachable verb, from the rows COMMAND itself answers from."""
    verbs = set()
    for path in sorted(VERB_META.glob("*.rs")):
        verbs |= set(re.findall(r'^    v\("([A-Z][A-Z0-9._|-]*)"', path.read_text(), re.M))
    if not verbs:
        sys.exit("check_command_coverage: no verbs parsed from VERB_META — the pattern has rotted")
    return verbs


def refresh(data: dict) -> dict:
    """Ask the pinned redis image what it serves. Needs docker."""
    pin = json.loads(ANCHORS.read_text())["anchors"]["redis"]["pinned"]
    image = f"redis:{pin}"
    script = ("redis-server --port 7399 --daemonize yes --save '' && "
              "for i in $(seq 50); do redis-cli -p 7399 ping >/dev/null 2>&1 && break; sleep 0.1; done && "
              "redis-cli -p 7399 COMMAND LIST")
    out = subprocess.run(["docker", "run", "--rm", "--entrypoint", "sh", image, "-c", script],
                         capture_output=True, text=True, timeout=180)
    cmds = sorted({c.strip().upper() for c in out.stdout.split() if c.strip()})
    if len(cmds) < 100:
        sys.exit(f"check_command_coverage: {image} answered {len(cmds)} commands — refusing a short list\n{out.stderr[:400]}")
    data["redis_command_set"] = {"version": pin, "captured": _today(), "commands": cmds}
    COVERAGE.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")
    print(f"captured {len(cmds)} commands from {image}")
    return data


def _today() -> str:
    return subprocess.run(["date", "-u", "+%Y-%m-%d"], capture_output=True, text=True).stdout.strip()


def classify(verb: str, table: dict) -> str:
    """The key in `table` whose any '|'-separated prefix matches, else ''."""
    for key in table:
        for pat in key.split("|"):
            pat = pat.strip()
            if verb == pat or (pat.endswith(".") and verb.startswith(pat)) or verb.startswith(pat + " "):
                return key
    return ""


def main() -> int:
    data = json.loads(COVERAGE.read_text())
    if "--refresh" in sys.argv:
        data = refresh(data)
    captured = data["redis_command_set"]
    if not captured["commands"]:
        sys.exit("check_command_coverage: no captured Redis command set — run with --refresh (needs docker)")

    ours, theirs = kevy_verbs(), set(captured["commands"])
    missing = sorted(theirs - ours)
    exempt, planned, unclassified = [], [], []
    for verb in missing:
        if classify(verb, data["exempt"]):
            exempt.append(verb)
        elif classify(verb, data["planned"]):
            planned.append(verb)
        else:
            unclassified.append(verb)

    print(f"command coverage — kevy {len(ours)} verbs vs redis {captured['version']} "
          f"{len(theirs)} commands (captured {captured['captured']})")
    print(f"  exempt        {len(exempt):>4}  (decisions already written down)")
    print(f"  planned       {len(planned):>4}  (a gap with an RFC that owns it)")
    print(f"  UNCLASSIFIED  {len(unclassified):>4}")
    if "--list" in sys.argv:
        for name, group in (("exempt", exempt), ("planned", planned), ("unclassified", unclassified)):
            if group:
                print(f"\n{name}:\n  " + "\n  ".join(group))
    for verb in unclassified:
        print(f"  UNCLASSIFIED {verb} — neither implemented nor decided about", file=sys.stderr)
    return 1 if unclassified else 0


if __name__ == "__main__":
    sys.exit(main())

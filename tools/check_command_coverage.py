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
import time
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
    """Ask the pinned redis image what it serves. Needs docker.

    Through the image's OWN entrypoint, in a detached container. Starting
    `redis-server` by hand under `--entrypoint sh` looks like the same
    question and is not: the modules Redis 8 ships — the query engine,
    JSON, time series, the probabilistic set — are loaded by the image's
    startup, so a hand-started server answers 449 commands where the real
    one answers 669. That reading is what made redis-stack look like the
    only image with a query engine.
    """
    pin = json.loads(ANCHORS.read_text())["anchors"]["redis"]["pinned"]
    image, name = f"redis:{pin}", "kevy-cmdcap"
    subprocess.run(["docker", "rm", "-f", name], capture_output=True)
    up = subprocess.run(["docker", "run", "-d", "--name", name, image],
                        capture_output=True, text=True, timeout=300)
    if up.returncode != 0:
        sys.exit(f"check_command_coverage: could not start {image}\n{up.stderr[:400]}")
    try:
        for _ in range(60):
            ping = subprocess.run(["docker", "exec", name, "redis-cli", "ping"],
                                  capture_output=True, text=True)
            if "PONG" in ping.stdout:
                break
            time.sleep(0.5)
        out = subprocess.run(["docker", "exec", name, "redis-cli", "COMMAND", "LIST"],
                             capture_output=True, text=True, timeout=120)
    finally:
        subprocess.run(["docker", "rm", "-f", name], capture_output=True)
    cmds = sorted({c.strip().upper() for c in out.stdout.split() if len(c.strip()) > 1})
    if len(cmds) < 400:
        sys.exit(f"check_command_coverage: {image} answered {len(cmds)} commands — refusing a short list")
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
            # Redis names a container's subcommands "ACL|CAT", and the
            # module families by a dotted prefix ("FT.SEARCH").
            if (verb == pat or verb.startswith(pat + "|")
                    or (pat.endswith(".") and verb.startswith(pat))
                    or verb.startswith(pat + " ")):
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
    # Redis lists a container's subcommands individually ("XGROUP|CREATE");
    # VERB_META names the container once ("XGROUP") and documents its
    # syntax there. A container we implement covers its subcommands, so
    # fold them before diffing — otherwise XGROUP alone reads as six gaps.
    missing = sorted(v for v in theirs - ours if v.split("|")[0] not in ours)
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

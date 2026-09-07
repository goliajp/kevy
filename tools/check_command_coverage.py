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
import os
import pathlib
import re
import subprocess
import time
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
COVERAGE = ROOT / "bench" / "COMMAND-COVERAGE.json"
# A public verb. Leading-underscore names (_FT.CONFIG, _FT.DEBUG) are Redis's
# internal surface and are not a compatibility question; they were filtered by
# the hand-run that produced the data file and not by this code, so a refresh
# would have moved 70 verbs into UNCLASSIFIED with nothing upstream changing.
VERB_SHAPE = re.compile(r"[A-Z][A-Z0-9._|-]*")
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
    image, name = f"redis:{pin}", f"kevy-cmdcap-{os.getpid()}"
    subprocess.run(["docker", "pull", "-q", image], capture_output=True, timeout=600)
    up = subprocess.run(["docker", "run", "-d", "--name", name, image],
                        capture_output=True, text=True, timeout=300)
    if up.returncode != 0:
        sys.exit(f"check_command_coverage: could not start {image}\n{up.stderr[:400]}")
    try:
        for _ in range(60):
            if "PONG" in _exec(name, "ping").stdout:
                break
            time.sleep(0.5)
        # Ask the container what it is. Writing the intended pin as the
        # captured version is the defect COMPETITOR-ANCHORS.json was opened
        # about: a stale cached layer would file 8.10.0's surface under 8.10.1.
        info = _exec(name, "INFO", "server").stdout
        served = next((l.split(":", 1)[1].strip() for l in info.splitlines()
                       if l.startswith("redis_version:")), "")
        if served != pin:
            sys.exit(f"check_command_coverage: {image} reports {served!r}, not the pinned {pin}")
        out = _exec(name, "COMMAND", "LIST")
    finally:
        subprocess.run(["docker", "rm", "-f", name], capture_output=True)
    if out.returncode != 0:
        sys.exit(f"check_command_coverage: COMMAND LIST failed\n{out.stderr[:400]}")
    # Shape, not magnitude. The `< 400` floor let the incident that opened
    # this file through: the module-less server answered 449. A verb is
    # upper-case after folding, and never a word out of an English log line.
    cmds = sorted({c.strip().upper() for c in out.stdout.split()
                   if VERB_SHAPE.fullmatch(c.strip().upper())})
    if not any(c.startswith("FT.") for c in cmds):
        sys.exit(f"check_command_coverage: {image} served {len(cmds)} commands but no FT.* — "
                 "the query engine did not load, so this is not the full surface")
    data["redis_command_set"] = {"version": served, "captured": _today(), "commands": cmds}
    COVERAGE.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")
    print(f"captured {len(cmds)} commands from {image} (reports {served})")
    return data


def _exec(name: str, *argv: str) -> subprocess.CompletedProcess:
    return subprocess.run(["docker", "exec", name, "redis-cli", *argv],
                          capture_output=True, text=True, timeout=120)


def _today() -> str:
    return subprocess.run(["date", "-u", "+%Y-%m-%d"], capture_output=True, text=True).stdout.strip()


def matches(verb: str, pat: str) -> bool:
    """One prefix against one verb. A dotted prefix ("FT.") names a family;
    everything else names a verb or a container whose subcommands it covers."""
    if pat.endswith("."):
        return verb.startswith(pat)
    return verb == pat or verb.startswith(pat + "|")


def classify(verb: str, table: dict) -> str:
    """The key whose prefix list matches this verb, else ''."""
    for key, entry in table.items():
        for pat in entry["prefixes"]:
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
    # The snapshot must be of the version we currently pin. Without this the
    # gate keeps answering from an old photograph after the anchor is raised —
    # green, printing the old version, reporting the old count. That is the
    # defect this whole family of gates was opened about, and it was in the
    # judge for layer 2 itself.
    pinned = json.loads(ANCHORS.read_text())["anchors"]["redis"]["pinned"]
    if captured["version"] != pinned:
        sys.exit(f"check_command_coverage: the captured set is redis {captured['version']} "
                 f"but the anchor pins {pinned} — run --refresh (needs docker)")

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
    # A planned gap must name an RFC. Whether that file is READABLE here is a
    # different question: .claude/ is deliberately not carried by git (the
    # owner's "git carries the user surface" decision), so a clone has the
    # entry and not the document. Requiring the file made every planned row
    # unowned on the bench box while being green on the workstation — the
    # gate answering a question about the checkout rather than about the
    # decision. It requires the reference; it verifies the file where one
    # exists to verify.
    for key, entry in data["planned"].items():
        rfc = entry.get("rfc", "")
        if not rfc:
            print(f"  MISSING RFC planned[{key}] names no document — a gap with nothing "
                  f"to point at is not owned, it is deferred", file=sys.stderr)
            unclassified = unclassified + [f"(planned:{key})"]
        elif (ROOT / ".claude").is_dir() and not (ROOT / rfc).exists():
            print(f"  MISSING RFC planned[{key}] names {rfc!r}, which does not exist in a "
                  f"checkout that does carry .claude/", file=sys.stderr)
            unclassified = unclassified + [f"(planned:{key})"]
    ceiling = data.get("unclassified_ceiling")
    for verb in unclassified:
        print(f"  UNCLASSIFIED {verb} — neither implemented nor decided about", file=sys.stderr)
    # Dead prefixes: an exemption that matches nothing looks like a decision
    # and is not one. Four were sitting in this file.
    live = {classify(v, data["exempt"]) for v in missing} | {classify(v, data["planned"]) for v in missing}
    for table, label in ((data["exempt"], "exempt"), (data["planned"], "planned")):
        for key, entry in table.items():
            for pat in entry["prefixes"]:
                if not any(matches(v, pat) for v in theirs):
                    print(f"  DEAD PREFIX {label}[{key}] lists {pat!r}, which matches nothing "
                          f"redis {captured['version']} serves", file=sys.stderr)
    if ceiling is not None and len(unclassified) > ceiling:
        print(f"  RATCHET unclassified {len(unclassified)} > ceiling {ceiling} — a new Redis "
              f"release, or a decision was removed. Classify them or raise the ceiling "
              f"deliberately.", file=sys.stderr)
        return 1
    if ceiling is not None and len(unclassified) < ceiling:
        print(f"  RATCHET unclassified {len(unclassified)} < ceiling {ceiling} — lower the "
              f"ceiling in bench/COMMAND-COVERAGE.json to lock the gain in.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

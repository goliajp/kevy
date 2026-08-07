#!/bin/bash
# propgate — a durable effect is a propagated effect, unless it is named.
#
#   bash bench/propgate.sh
#
# The reason this exists is three data-loss bugs on one branch, all the
# same shape: a write reached the AOF and never reached the replica.
# Multi-key `DEL`/`UNLINK`, `MSET`, the cross-shard `RENAME` and `LMOVE`
# two-steps and the `*STORE` destinations were durable and unreplicated,
# because the replication push lived on the single-key dispatch path and
# those ops do not take it. The change feed reads the replication
# backlog, so the same gap also meant no CDC frame — one omission, three
# faces.
#
# The fix paired them in `log_effect`. What this gate adds is that the
# pairing cannot quietly come apart again: every `self.log(…)` /
# `self.log_write(…)` in the runtime must either be followed by a
# `push_mutation` within a few lines, or be listed below with the reason
# it is durable-only. A new fast path that logs and forgets to push
# fails here rather than in someone's replica.
#
# It is a source lint, in the family of locgate and commentgate — it
# reads text, not behaviour. `repligate.sh` is the behavioural gate; the
# two answer different questions and neither replaces the other.
#
# Exit codes: 0 = PASS, 1 = an unpaired, unnamed call site.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/.." && pwd)

python3 - "$ROOT" <<'PY'
import pathlib
import re
import sys

root = pathlib.Path(sys.argv[1])
src = root / "crates" / "kevy-rt" / "src"

# Durable-only on purpose. Each entry is a file and the reason, and the
# reason is the point: "it was like that" is not one.
NAMED = {
    "exec_mutated.rs": "inside log_effect itself — this call IS the durable half of the pair",
    "exec.rs": "inside log / log_write and their TTL follow-ups — the helpers, not call sites",
    "exec_op.rs": "FLUSHALL propagates by bumping the feed generation; a record would land at "
                  "the offset the bump just reset to zero (pinned by feed_cdc)",
    "exec_dispatch.rs:tick": "window-tick SEGMENTED frames — a replica runs its own window tick "
                             "over its own data dir and seals its own segments",
}

CALL = re.compile(r"self\.(log|log_write)\s*\(")
PUSH = re.compile(r"push_mutation\s*\(")
# How far to look for the pairing, counted in **code** lines. Comments do
# not count: the push beside the main dispatch path sits fifteen lines
# below its log_write, thirteen of which explain why the push is
# suppressed while applying a replicated frame. Counting raw lines would
# have meant tuning a number until the gate went green, which is how a
# gate stops meaning anything.
WINDOW = 8
BOUNDARY = re.compile(r"^\s*(pub(\(crate\))?\s+)?(async\s+)?fn\s")


def paired(lines, start):
    """Is there a push_mutation within WINDOW code lines, in this fn?"""
    seen = 0
    for line in lines[start + 1 :]:
        stripped = line.strip()
        if not stripped or stripped.startswith(("//", "/*", "*")):
            continue
        if BOUNDARY.match(line):
            return False  # a new function: out of scope
        if PUSH.search(line):
            return True
        seen += 1
        if seen >= WINDOW:
            return False
    return False

problems = []
checked = 0
for f in sorted(src.glob("*.rs")):
    lines = f.read_text(encoding="utf-8").splitlines()
    for i, line in enumerate(lines):
        if not CALL.search(line) or line.lstrip().startswith(("///", "//", "*")):
            continue
        # The definitions themselves are not call sites.
        if re.search(r"fn\s+(log|log_write|log_effect)\b", line):
            continue
        checked += 1
        if paired(lines, i):
            continue
        key = f.name
        if f.name == "exec_dispatch.rs" and "tick" in "\n".join(lines[max(0, i - 8) : i + 2]):
            key = "exec_dispatch.rs:tick"
        if key in NAMED:
            continue
        problems.append(f"{f.name}:{i + 1}: {line.strip()}")

if problems:
    print(f"propgate: FAIL — {len(problems)} durable write(s) with no propagation and no reason:")
    for p in problems:
        print(f"  {p}")
    print("  Either pair it (log_effect, or an explicit push_mutation beside it),")
    print("  or add it to NAMED in bench/propgate.sh with why it is durable-only.")
    sys.exit(1)

print(f"propgate: PASS — {checked} durable-write call site(s), each paired or named")
PY

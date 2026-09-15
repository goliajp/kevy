#!/usr/bin/env python3
"""The loom suites run, and every test in them runs.

Two loom suites exist in this tree — `kevy-ring/tests/loom.rs` (the SPSC
ring's producer/consumer handshake) and `kevy-rt/tests/loom.rs` (the
cross-shard park/wake fence). Between them they are the stated proof for
four `unsafe` atomic orderings, and three production sites name a loom
test in the comment that argues their correctness:

    shard_run.rs      "Loom-verified by `tests/loom.rs::park_wake_fence_*`"
    shard_flush.rs    "Loom-verified by `...::no_wake_implies_drained`"
    uring_park.rs     "same pairing as `Shard::run` / `flush_wakes`;
                       loom-verified there"

Nothing ran them. Both files are `#![cfg(loom)]`, and no gate, workflow,
Makefile or suite entry anywhere passed `--cfg loom`. What `cargo test
--workspace` did with them, every time, was:

    running 0 tests
    test result: ok. 0 passed; 0 failed; 0 ignored

which is a pass. Four exhaustive interleaving searches reported success
for months by not existing — the repository's own name for this shape is
a measuring device whose failure is indistinguishable from data.

**The floor comes from the source, not from a constant here.** Each
suite's expected test count is the number of `#[test]` functions in its
file, so the two ways this can rot both fail: the whole harness going
silent again (0 < 2), and someone adding a fifth loom test that the
`--cfg loom` build does not pick up (4 < 5). A hardcoded 4 would catch
only the first.

An abort is a failure. Loom reports a broken ordering by panicking inside
a destructor during model cleanup, which lands as SIGABRT and no
`test result:` line at all — so a missing summary is never read as
agreement.

Run: python3 tools/check_loom.py
Exit: 0 pass, 1 violation, 2 refused.
"""

import os
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
RUN_TIMEOUT_SECONDS = 1800

# Loom's default preemption bound is 2, and both suites' headers said raising
# it "explodes the state space combinatorially". Measured, it does not: the
# ring suite costs 0.00 / 0.01 / 0.02 / 0.04 s at bounds 2 / 3 / 4 / 5, and the
# park/wake suite stays at 0.00 s throughout. Four times almost nothing is
# still almost nothing, so the gate searches deeper than the default rather
# than inheriting a bound that was set for a cost that is not there.
LOOM_MAX_PREEMPTIONS = "5"


def refuse(msg: str) -> None:
    print(f"loomgate: REFUSED — {msg}")
    sys.exit(2)


def suites() -> list[tuple[str, int]]:
    """Every `#![cfg(loom)]` integration suite, with its declared test count."""
    found = []
    for path in sorted(ROOT.glob("crates/*/tests/loom.rs")):
        text = path.read_text()
        if "#![cfg(loom)]" not in text:
            refuse(f"{path.relative_to(ROOT)} is not `#![cfg(loom)]` — this "
                   "gate's whole premise is that these files are cfg-gated")
        declared = len(re.findall(r"^#\[test\]$", text, re.M))
        if declared == 0:
            refuse(f"{path.relative_to(ROOT)} declares no #[test] — an empty "
                   "suite is the state this gate exists to catch")
        found.append((path.parts[-3], declared))
    if not found:
        refuse("no crates/*/tests/loom.rs found — a gate that finds nothing "
               "must not report agreement")
    return found


def run(crate: str) -> tuple[int, int, str]:
    """Returns (passed, failed, raw output) for one suite under --cfg loom."""
    proc = subprocess.run(
        ["cargo", "test", "-p", crate, "--test", "loom", "--release"],
        cwd=ROOT,
        env={
            **os.environ,
            "RUSTFLAGS": "--cfg loom",
            "LOOM_MAX_PREEMPTIONS": LOOM_MAX_PREEMPTIONS,
        },
        capture_output=True,
        text=True,
        timeout=RUN_TIMEOUT_SECONDS,
    )
    out = proc.stdout + proc.stderr
    m = re.search(r"test result: \w+\. (\d+) passed; (\d+) failed", out)
    if not m:
        # No summary: the harness aborted (loom's signature failure) or never
        # linked. Either way it did not agree with anything.
        return 0, -1, out
    return int(m.group(1)), int(m.group(2)), out


def main() -> int:
    bad = []
    for crate, declared in suites():
        passed, failed, out = run(crate)
        if failed != 0:
            tail = "\n".join(out.strip().splitlines()[-6:])
            bad.append(f"{crate}: loom did not finish clean "
                       f"(passed={passed}, failed={failed})\n{tail}")
        elif passed < declared:
            bad.append(f"{crate}: {declared} #[test] in tests/loom.rs but only "
                       f"{passed} ran under --cfg loom")
        else:
            print(f"loomgate: {crate} {passed}/{declared} interleaving searches ran")

    if bad:
        print("loomgate: FAIL")
        for b in bad:
            print(f"  {b}")
        return 1
    print("loomgate: PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())

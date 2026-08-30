# v6.0.0 audit — what was verified, and what was not

Ordered by what a reader would want to know first: whether the release was
checked, then what the checks that had not run have to say.

## 1. Thirty of eighty-five checks had no evidence of running

`suite/manifest.toml` declares 85 checks in three tiers
(precommit 24 ⊆ prerelease 41 ⊆ full 85). Two things run them: CI, and a
person typing `python3 tools/suite.py <tier>` on a box.

What actually ran against the tree v6.0.0 shipped from:

| | |
|---|---|
| CI (58 jobs) | 43 of the 85 checks appear in `ci.yml` |
| `prerelease` on lx64, 2026-08-30 01:39 | 41 checks |
| union | **55** |
| **no evidence either way** | **30** |

The `full` tier last ran on **2026-08-17 23:08** — thirteen days and one
major release before v6.0.0 — and it ran **71** checks, because the tier
has grown by fourteen since. The record is
`lx64:/home/kevybench/kevy/target/suite-full.json`; nothing in git tracks
when a tier last passed, so nothing would have said so.

CI runs `python3 tools/suite.py --audit`, which verifies that the manifest
agrees with itself. That is the tree reading the tree. It cannot and does
not assert that any check ran.

The thirty, by what they need:

- **6 need an exclusive box** — arena, scalesoak, perfgate-median,
  kevy-ab, capacity-envelope, pgcompare. lx64 is shared: twelve
  containers belong to other services. Not runnable here.
- **1 needs a device** — mobilegate-all. The suite audit already says so
  out loud: `⊘ 'device' — runs in no environment this project has`.
- **23 are runnable** — `linux,server-release`, `server-debug`, or
  nothing. These are the audit's working set.

## 2. Findings

(filled as the twenty-three run)

### F1 — CRC32C exists twice, for a reason a third option refutes

`crates/kevy-persist/src/crc32c.rs` and `crates/kevy-vlog/src/crc32c.rs`
are 73 lines each and differ only in their doc comment. The clone atlas
ranks the pair third among cross-crate clusters (55 shared fingerprints).
The copy says why:

> kevy-vlog cannot depend on kevy-persist (kevy-persist -> kevy-store ->
> kevy-vlog would cycle), and the shared part is 40 lines.

Both halves of that are true, and the choice it presents — duplicate, or
cycle — is not the whole set. A pure checksum with no dependencies is a
stone by this project's own model, and this repository already extracts
stones that small (kevy-madvise, kevy-tmpdir). The hardware path is
already factored out into `kevy_sys::checksum`; what is duplicated is the
safe slicing-by-8 fallback, which exists in both crates because
`kevy-persist` depends on kevy-sys only under
`cfg(not(target_arch = "wasm32"))` and so has nothing on wasm.

A leaf crate carrying the table, depended on unconditionally by both,
removes the copy without creating the cycle. Cost: a 42nd published
crate, which is a door, a publish-order entry, and a version layer.


### F2 — a gate reported a failure it had not had

`bench/onrampgate.sh` waits `sleep 1.2` for two eight-shard servers to
come up. On this laptop they took **14.8 seconds** to accept. The seeder
connected into nothing, and because the script sets only `set -u`, it ran
to the end and printed:

    onrampgate: FAIL — import rate 0/s < 200k/s

That headline reads as a slow engine. The engine was never asked. The
server's output went to `/dev/null`, so the reason was not available
either — the servers were healthy the whole time, which is only visible
once that redirect is replaced with a file.

Fixed the way this repository already does it elsewhere: `availgate.sh`
waits on the port and keeps the server's output in `$DIR/*.out`, and
`kevy_testnet::wait_listening(port, timeout)` is the same idea on the Rust
side. onrampgate now polls for accept with a 60s ceiling and prints the
first twenty lines of both server logs if it runs out.

With the wait fixed the gate measures something real: it seeds 1,000,001
keys, exports them, and imports at 91,720/s against a 200k/s bar — on a
laptop that needed 14.8s to bind and 153s to export. That is a bar being
judged on a machine it was not set on, not a regression; the verdict that
counts comes from the bench box.

**The pattern, counted:** 56 of 105 scripts under `bench/` both wait with
a fixed `sleep` and send the server's output to `/dev/null`. Only the one
that demonstrably broke is changed here. Rewriting fifty-six scripts that
cannot be run to verify would be worse than leaving them: the two correct
examples are named above, and the next one to bite has somewhere to look.


### A note on how this audit ran

The first batch of gates was invoked by name — `bash bench/idxgate.sh` —
and seven of them exited 1 in zero seconds. That looked like seven broken
gates. It was one broken harness: those gates take the binary as an
argument and said so, in a usage line, which is the correct behaviour.
`suite/manifest.toml` carries the exact command for every check; the
second batch uses it. Guessing an invocation and reading the refusal as a
finding is the same error this audit is about, made by the auditor.


## 3. The last full run failed on three checks, and nothing carried it forward

`lx64:/home/kevybench/kevy/target/suite-full.json`, 2026-08-17 23:08:

    PASS 58 · NOT-RUN 7 · ADVISORY 3 · FAIL 3
    FAIL   tailgate     773.3s
    FAIL   agggate       14.1s
    FAIL   onrampgate    46.8s

v6.0.0 shipped thirteen days later. Nothing in the tree records those
three failures, and nothing asked about them again.

Re-run today against the tree v6.0.0 shipped from, on the same box:

| 2026-08-17 | 2026-08-30 |
|---|---|
| `agggate` FAIL | **PASS**, 16s — fixed itself somewhere in thirteen days, and nobody knew either way |
| `onrampgate` FAIL | **PASS**, 91s — after F2, the fixed `sleep 1.2`, which this audit found independently before reading this record |
| `tailgate` FAIL | still open — the one noisy gate judging an absolute, and it needs an exclusive box this one is not |

`tiergate` is ADVISORY, not FAIL: fourteen PENDING acceptance lines whose
measurements have not been taken, declared as such, and the runner does
not let it block. It is not a permanently-red gate — a thing this
repository is careful about elsewhere and was careful about here too.

The seven NOT-RUN rows on 2026-08-17 are the honest kind: md-port,
wasm-size, sitegate, site-commands, site-parity (node/chromium),
mobilegate-all (device), fuzz-smoke (ci). The runner said "full minus
these" out loud, which is its stated contract.


## 4. F5 — `import --resume` loses data after a kill, and says it did not

`drill-mailrs` is a `case`-tier check that had never run against this
tree. Step 5 kills the importer mid-flight and resumes:

    src digest: 200000 keys 787e8f923e572af2
    dst digest: 191944 keys 8c1a4065eadf79aa
    src dbsize: 290025   dst dbsize: 278451
    imported: 604962 ok, 0 errors, offset 736562209

`--strict` did not fire. The resume read the whole dump — its recorded
offset equals the file's length — reported zero errors, and left the
destination short by ~12,000 keys.

**Rate, measured properly:** six runs, sequential, exclusive, with the box
verified quiet between each — **three mismatches in six**. The plain
import in step 3 was correct in all six; only kill-then-resume loses.

**Shape:** 12,842 keys missing, spread from suffix 35 to 199,973 in
**12,012 separate runs** — scattered numerically, with **zero** extra keys
in the destination. The dump is written by `export`, which walks in hash
order, so numerically scattered is what a contiguous stretch of the dump
looks like from the key side. Nothing is duplicated, which rules out
double application; something is skipped.

### What I got wrong on the way here, twice worth writing down

**A rate measured under my own interference.** I first reported "3 of 8
(33%)" — measured while several hunts I had launched were overlapping on
the same box. The drill uses fixed ports 7071/7072 and fixed /tmp paths;
concurrent drills kill each other's servers. `/tmp/drill-dst-raw.txt` was
14 bytes on one of those runs, which is what that looks like. The number
was not a rate of anything. The 3-in-6 above is sequential and exclusive.

**An edit I believed had landed.** One patch's assertion matched other
text, so the change was never written — and I copied the unchanged file to
the box and reasoned on top of it. `grep` on both sides is what caught it.

Five instrument errors in this investigation, all mine: counting processes
with a pattern that matches the counting command; `ls a* b*` aborting in
zsh when the second glob has no match; diffing `KEYS` output whose row
numbers differ between servers; the unlanded edit; the contaminated rate.
Each was caught, and each was caught *after* a wrong number had been said
out loud. That is the same defect this audit is about, in the auditor.


### F5, resolved: the drill was manufacturing the loss it reported

The engine is not losing acknowledged writes. Tested in isolation against
a fresh server: 1,000 pipelined `SET`s, 1,000 `+OK` replies read back,
then `DBSIZE` on a **new** connection and on the same one — **1000 and
1000**. An acknowledged write is immediately visible.

What the drill actually does in step 5:

    DPID=$(start_server $DST "$DIR/dst")   # server started inside $( )
    ...
    kill $DPID 2>/dev/null; wait $DPID     # wait returns instantly
    rm -rf "$DIR/dst"; mkdir -p "$DIR/dst"
    DPID=$(start_server $DST "$DIR/dst")   # new server, same port

`wait` can only wait for this shell's own children. The server is a child
of the command-substitution subshell, so `wait $DPID` waits for nothing
and returns immediately. `kill` has been sent and not collected.

kevy binds one socket per shard with `SO_REUSEPORT`. The new server binds
**alongside** the dying one, and the kernel distributes the importer's
connections across both. Writes routed to the dying server are executed
and acknowledged, and then leave with it. The importer — correctly, by its
own accounting — records those bytes as durable and fsyncs the offset;
`--resume` starts after them; nobody ever sends them again.

That is every observation, including the ones that made no sense
separately: `progress at kill = 608,727` with `DBSIZE = 0`; the resume
importing exactly 512 commands fewer than a complete import, which is what
608,727 bytes comes to; ~12,000 keys missing, scattered by key name and
contiguous in the dump's hash order; nothing duplicated; zero errors
anywhere; and step 3's plain import — which restarts no server — correct
every single time.

`availgate.sh` has carried a `wait_ports_free()` for this since before
this release, with a comment naming the phenomenon: "a just-killed
server's sockets can linger a beat… an unnamed squatter cost a debugging
session once." The drill did not use it.

**Fixed** with the same idea, in the drill's own vocabulary: `stop_server`
kills and then waits for the process to actually be gone, `wait_port_free`
waits until nothing is listening, and `start_server` refuses to bind until
that is true.

    before   6 sequential exclusive runs → 3 mismatches
    after    6 sequential exclusive runs → 0

**What this finding is worth.** A `case`-tier gate that had never been run
against the tree v6.0.0 shipped from reported, on its first run, that
crash-resume loses twelve thousand keys with no error. Left alone it would
have become either a false conviction of the engine or a gate switched off
for flakiness. It was neither. The engine is clean and the harness is
fixed, and both of those are now demonstrated rather than assumed.

#!/usr/bin/env python3
"""Everything CI checks on a push can be checked before the push, or says why not.

The ratchets CI runs — covgate, deadgate, doctestgate, the doctest run, the
feature-lint clippy, the generated-docs check — were in no tier a developer
runs, so merges that were green locally went red on develop, several in a
row. The `premerge` tier holds what CI runs; this keeps it that way.

Every verdict-bearing command in `.github/workflows/ci.yml` must either be
the command of a manifest row in `premerge` or below, or be listed under
`[suite.ci_only]` with the reason it cannot run there. A key matches its
command exactly, or every command it prefixes when it ends in ` *` — a
bare `cargo test` must not excuse the next unlisted `cargo test -p …`. A
listed command that CI no longer runs fails too: an exemption must not
outlive its step.

`cargo build` lines are prerequisites, not verdicts: a row that needs a
binary builds it or declares the requirement.

Floor rule: finding no CI commands is a broken reader, not a pass.

Run: python3 tools/check_ci_parity.py
Exit: 0 pass, 1 violation, 2 refused.
"""

import pathlib
import re
import shlex
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
CI = ROOT / ".github/workflows/ci.yml"
MANIFEST = ROOT / "suite/manifest.toml"
TIERS = ["precommit", "premerge", "prerelease", "full"]
VERDICT = re.compile(r"^(cargo |bash bench/|python3 tools/)")
ENV_ASSIGN = re.compile(r"^[A-Z_][A-Z0-9_]*=")


def refuse(msg):
    print(f"ci-parity: REFUSED — {msg}")
    sys.exit(2)


def norm(cmd):
    """One comparable form: no quiet flags, no leading env assignments, and
    a bench script is itself whatever arguments it is handed."""
    toks = [t for t in cmd.split() if t != "-q"]
    while toks and ENV_ASSIGN.match(toks[0]):
        toks.pop(0)
    if len(toks) >= 2 and toks[0] == "bash" and toks[1].startswith("bench/"):
        return f"bash {toks[1]}"
    return " ".join(toks)


def ci_commands():
    out = []
    for n, line in enumerate(CI.read_text(encoding="utf-8").splitlines(), 1):
        s = re.sub(r"^(-\s+)?run:\s*", "", line.strip())
        if VERDICT.match(s) and not s.startswith("cargo build"):
            out.append((n, norm(s.rstrip("\\").strip())))
    return out


def row_commands(cmd):
    """A row's command, or each command inside its `bash -c '…'`."""
    toks = shlex.split(cmd)
    if toks[:2] == ["bash", "-c"] and len(toks) > 2:
        return [norm(part) for part in re.split(r"&&|;", toks[2]) if part.strip()]
    return [norm(cmd)]


def excuses(key, cmd):
    if key.endswith(" *"):
        return cmd.startswith(key[:-1])
    return cmd == key


def main():
    m = tomllib.loads(MANIFEST.read_text(encoding="utf-8"))
    rank = {t: i for i, t in enumerate(TIERS)}
    rows = [(c["id"], c["tier"], rc) for c in m["check"] for rc in row_commands(c["cmd"])]
    exempt = m["suite"].get("ci_only", {})
    seen = ci_commands()
    if not seen:
        refuse(f"read no commands from {CI.relative_to(ROOT)}")
    bad, used = [], set()
    for line, cmd in seen:
        hits = [(i, t) for i, t, rc in rows if cmd == rc or cmd.startswith(rc + " ")]
        if hits:
            late = [i for i, t in hits if rank[t] > rank["premerge"]]
            if len(late) == len(hits):
                bad.append(f"ci.yml:{line}: `{cmd}` is row {late[0]}, which runs only in "
                           f"{hits[0][1]} — CI runs it on every push")
            continue
        key = next((k for k in exempt if excuses(k, cmd)), None)
        if key is None:
            bad.append(f"ci.yml:{line}: `{cmd}` has no manifest row and no [suite.ci_only] reason")
        elif not exempt[key].strip():
            bad.append(f"[suite.ci_only] `{key}` has no reason")
        else:
            used.add(key)
    for k in sorted(set(exempt) - used):
        bad.append(f"[suite.ci_only] `{k}` matches nothing CI runs any more")
    for b in bad:
        print(f"ci-parity: {b}")
    if bad:
        return 1
    print(f"ci-parity: ok — {len(seen)} CI commands, each in a row by premerge "
          f"or under one of {len(used)} CI-only reasons")
    return 0


if __name__ == "__main__":
    sys.exit(main())

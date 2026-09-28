#!/usr/bin/env python3
"""Everything CI checks on a push can be checked before the push, or says why not.

The ratchets CI runs — covgate, deadgate, doctestgate, the doctest run, the
feature-lint clippy, the generated-docs check — were in no tier a developer
runs, so merges that were green locally went red on develop, several in a
row. The `premerge` tier holds what CI runs; this keeps it that way.

Every verdict-bearing command in `.github/workflows/ci.yml` must either be
the command of a manifest row in `premerge` or below (`prerelease` for a job
CI runs only on release and hotfix branches), or be listed under
`[suite.ci_only]` with the reason it cannot run there. A key matches its
command exactly, or every command it prefixes when it ends in ` *` — a
bare `cargo test` must not excuse the next unlisted `cargo test -p …`. A
listed command that CI no longer runs fails too: an exemption must not
outlive its step.

A step's `env:` is part of its command: CI ran compressgate with
COMPRESSGATE_UNIT_ONLY=1 while the row ran it without, and failed on lines
CI never asks about. Every variable a CI step sets must be set by the row's
command too (credentials excepted).

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
# set by CI for its own access, not part of what the check checks
CREDENTIALS = {"GITHUB_TOKEN"}


def refuse(msg):
    print(f"ci-parity: REFUSED — {msg}")
    sys.exit(2)


def norm(cmd):
    """One comparable form: no quiet flags, no leading env assignments, and
    a bench script is itself whatever arguments it is handed."""
    cmd = re.split(r" \|\|| \| | 2>&1", cmd)[0]
    toks = [t for t in shlex.split(cmd) if t != "-q"]
    while toks and ENV_ASSIGN.match(toks[0]):
        toks.pop(0)
    if len(toks) >= 2 and toks[0] == "bash" and toks[1].startswith("bench/"):
        return f"bash {toks[1]}"
    return " ".join(toks)


def ci_commands():
    """(line, command, env keys the command runs under, the tier it needs)."""
    out, env, env_indent, tier = [], set(), None, "premerge"
    for n, line in enumerate(CI.read_text(encoding="utf-8").splitlines(), 1):
        indent = len(line) - len(line.lstrip())
        if re.match(r"^  [a-z0-9_-]+:\s*$", line):
            tier = "premerge"
        if re.match(r"^    if:.*refs/heads/release/", line):
            tier = "prerelease"
        if re.match(r"^\s*- ", line):
            env, env_indent = set(), None
        if env_indent is not None:
            m = re.match(r"^\s+([A-Z_][A-Z0-9_]*):", line)
            if m and indent > env_indent:
                env.add(m.group(1))
                continue
            env_indent = None
        if re.match(r"^\s+env:\s*$", line):
            env_indent = indent
            continue
        s = re.sub(r"^(-\s+)?run:\s*", "", line.strip())
        inline = set(re.findall(r"^([A-Z_][A-Z0-9_]*)=", s))
        s = re.sub(r"^([A-Z_][A-Z0-9_]*=\S*\s+)+", "", s)
        if VERDICT.match(s) and not s.startswith("cargo build"):
            out.append((n, norm(s.rstrip("\\").strip()), (env | inline) - CREDENTIALS, tier))
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
    rows = [(c["id"], c["tier"], rc, c["cmd"]) for c in m["check"] for rc in row_commands(c["cmd"])]
    exempt = m["suite"].get("ci_only", {})
    seen = ci_commands()
    if not seen:
        refuse(f"read no commands from {CI.relative_to(ROOT)}")
    bad, used = [], set()
    for line, cmd, env, need in seen:
        hits = [(i, t, raw) for i, t, rc, raw in rows if cmd == rc]
        if hits:
            late = [i for i, t, _ in hits if rank[t] > rank[need]]
            if len(late) == len(hits):
                bad.append(f"ci.yml:{line}: `{cmd}` is row {late[0]}, which runs only in "
                           f"{hits[0][1]} — CI runs it by {need}")
            early = [i for i, t, _ in hits if need == "prerelease" and rank[t] < rank[need]]
            for i in early:
                bad.append(f"ci.yml:{line}: row {i} runs `{cmd}` before prerelease, but CI "
                           f"runs it only on release and hotfix branches")
            for i, _, raw in hits:
                missing = sorted(k for k in env if f"{k}=" not in raw)
                if missing:
                    bad.append(f"ci.yml:{line}: CI runs `{cmd}` with {', '.join(missing)} set; "
                               f"row {i} does not")
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
    print(f"ci-parity: ok — {len(seen)} CI commands, each in a row by the tier CI runs it at "
          f"or under one of {len(used)} CI-only reasons")
    return 0


if __name__ == "__main__":
    sys.exit(main())

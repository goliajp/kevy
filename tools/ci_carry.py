#!/usr/bin/env python3
"""Which suite rows CI has already run on this exact commit.

A release ran the premerge rows twice: once on CI for the commit, and again
on the box as part of prerelease — the same commands on the same code (the
manifest's premerge tier is defined as what CI checks on a push). The second
run cost the better part of an hour a release and could only agree with the
first.

So a tier above premerge carries those rows from CI instead of re-running
them, and only when that is the same question already answered:

- the working tree has no tracked changes, so HEAD is what CI built;
- CI's run for HEAD's commit finished, and for a row inherited from
  premerge or below, the whole run succeeded;
- a row that names a `ci_job` (a check CI runs in one job of its own, on
  release branches for instance) is carried when that job succeeded.

Anything else runs here as before. `gh` answers for GitHub; when it cannot
answer (no network, no auth), nothing is carried and the tier says why.
"""

import json
import subprocess

TIER_RANK = {"precommit": 0, "premerge": 1, "prerelease": 2, "full": 3}
CI_WORKFLOW = ".github/workflows/ci.yml"


def _run(cmd):
    r = subprocess.run(cmd, capture_output=True, text=True)
    return r.returncode, r.stdout


def _head(root):
    code, sha = _run(["git", "-C", str(root), "rev-parse", "HEAD"])
    if code != 0:
        return None, "git rev-parse failed"
    code, dirty = _run(["git", "-C", str(root), "status", "--porcelain", "--untracked-files=no"])
    if code != 0 or dirty.strip():
        return None, "the tree has tracked changes, so CI did not build what would run here"
    return sha.strip(), ""


def _ci_run(sha):
    code, out = _run(["gh", "api", f"repos/{{owner}}/{{repo}}/actions/runs?head_sha={sha}&per_page=50"])
    if code != 0:
        return None, "gh could not list CI runs"
    runs = [r for r in json.loads(out).get("workflow_runs", [])
            if r.get("path") == CI_WORKFLOW and r.get("status") == "completed"]
    if not runs:
        return None, f"no finished CI run for {sha[:9]}"
    return max(runs, key=lambda r: (r.get("run_attempt", 1), r.get("updated_at", ""))), ""


def _jobs(run_id):
    jobs, page = {}, 1
    while True:
        code, out = _run(["gh", "api",
                          f"repos/{{owner}}/{{repo}}/actions/runs/{run_id}/jobs?per_page=100&page={page}"])
        if code != 0:
            return None
        batch = json.loads(out).get("jobs", [])
        jobs.update({j["name"]: j.get("conclusion") for j in batch})
        if len(batch) < 100:
            return jobs
        page += 1


def carried(root, checks, tier):
    """`({check id: reason}, why-none)` — the rows this tier may take from
    CI, or an empty map and the reason nothing was carried."""
    if TIER_RANK.get(tier, 0) <= TIER_RANK["premerge"]:
        return {}, ""
    sha, why = _head(root)
    if sha is None:
        return {}, why
    run, why = _ci_run(sha)
    if run is None:
        return {}, why
    jobs = _jobs(run["id"])
    if jobs is None:
        return {}, "gh could not list the CI run's jobs"
    green = run.get("conclusion") == "success"
    where = f"CI run {run['id']} on {sha[:9]}"
    out = {}
    for c in checks:
        if c.get("ci_job"):
            if jobs.get(c["ci_job"]) == "success":
                out[c["id"]] = f"{where}, job {c['ci_job']!r}"
        elif green and TIER_RANK[c["tier"]] <= TIER_RANK["premerge"]:
            out[c["id"]] = where
    why = "" if green else f"{where} concluded {run.get('conclusion')}, so premerge rows run here"
    return out, why

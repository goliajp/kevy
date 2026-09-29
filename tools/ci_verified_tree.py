#!/usr/bin/env python3
"""Whether CI already verified the exact tree a release tag points at.

The release workflow's verify job rebuilt the workspace with fat LTO, ran
the release-profile tests and every doc example — the same commands CI
had just run on the release branch for the same code, and half an hour
in front of the publish chain. When CI's run for this tree succeeded,
including the release-profile test job that only runs on release
branches, that answer is carried; otherwise verify runs everything.

The tree, not the commit: a tag on a merge commit carries the release
branch's tree unchanged, and CI ran on the branch head. Candidates are
HEAD and those of its parents whose tree is identical.

Prints `proven=true|false` and `why=...` lines for $GITHUB_OUTPUT.
Needs GITHUB_REPOSITORY and GH_TOKEN (actions: read).
"""

import json
import os
import subprocess
import urllib.request

CI_WORKFLOW = ".github/workflows/ci.yml"
REQUIRED_JOBS = ("test (release profile, Linux)",)


def git(*args):
    return subprocess.run(["git", *args], capture_output=True, text=True).stdout.strip()


def api(path):
    req = urllib.request.Request(
        f"https://api.github.com/repos/{os.environ['GITHUB_REPOSITORY']}/{path}",
        headers={"Authorization": f"Bearer {os.environ['GH_TOKEN']}",
                 "Accept": "application/vnd.github+json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)


def candidates():
    tree = git("rev-parse", "HEAD^{tree}")
    parents = git("rev-list", "--parents", "-n", "1", "HEAD").split()[1:]
    return [git("rev-parse", "HEAD")] + [p for p in parents
                                         if git("rev-parse", f"{p}^{{tree}}") == tree]


def verdict_for(sha):
    runs = [r for r in api(f"actions/runs?head_sha={sha}&per_page=50")["workflow_runs"]
            if r.get("path") == CI_WORKFLOW and r.get("status") == "completed"]
    if not runs:
        return None
    run = max(runs, key=lambda r: (r.get("run_attempt", 1), r.get("updated_at", "")))
    if run.get("conclusion") != "success":
        return False, f"CI run {run['id']} on {sha[:9]} concluded {run.get('conclusion')}"
    jobs, page = {}, 1
    while True:
        batch = api(f"actions/runs/{run['id']}/jobs?per_page=100&page={page}")["jobs"]
        jobs.update({j["name"]: j.get("conclusion") for j in batch})
        if len(batch) < 100:
            break
        page += 1
    missing = [j for j in REQUIRED_JOBS if jobs.get(j) != "success"]
    if missing:
        return False, f"CI run {run['id']} on {sha[:9]} did not pass {missing}"
    return True, f"CI run {run['id']} passed on {sha[:9]}, same tree"


def main():
    why = "no finished CI run for this tree"
    for sha in candidates():
        v = verdict_for(sha)
        if v is None:
            continue
        ok, why = v
        if ok:
            print(f"proven=true\nwhy={why}")
            return
    print(f"proven=false\nwhy={why}")


if __name__ == "__main__":
    main()

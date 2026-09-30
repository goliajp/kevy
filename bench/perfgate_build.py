"""perfgate's binaries: a path is used as given, anything else is a git rev.

  path/to/kevy          that file
  v6.4.0, HEAD, branch  kevy built from that commit (release-perf profile)
  HEAD+kevy-alloc       the same, with cargo features (comma separated)
  merge-base            where HEAD left origin/develop; on develop itself,
                        the last release tag
  last-release          the newest v* tag reachable from HEAD

Builds happen in a detached worktree under bench/.perfgate-ref/ and are
cached there by commit and features, so naming a rev twice builds it once.
"""

import os
import pathlib
import shutil
import subprocess

HERE = pathlib.Path(__file__).resolve().parent
REPO = HERE.parent
CACHE = HERE / ".perfgate-ref"


def git(*args):
    return subprocess.run(["git", "-C", str(REPO), *args], capture_output=True,
                          text=True, check=True).stdout.strip()


def last_release():
    return git("describe", "--tags", "--abbrev=0", "--match", "v[0-9]*", "HEAD")


def resolve_rev(rev):
    """(name to print, commit sha)"""
    name = rev
    if rev == "last-release":
        rev = last_release()
        name = rev
    elif rev == "merge-base":
        head = git("rev-parse", "HEAD")
        mb = git("merge-base", "HEAD", "origin/develop")
        rev = mb if mb != head else last_release()
        name = "merge-base with origin/develop" if mb != head else rev
    return name, git("rev-parse", "--verify", f"{rev}^{{commit}}")


def split_spec(spec):
    rev, _, feats = spec.partition("+")
    return rev, [f for f in feats.split(",") if f]


def is_path(spec):
    return os.sep in spec and os.path.isfile(spec)


def cached(sha, feats):
    tag = f"-{'+'.join(feats)}" if feats else ""
    return CACHE / f"kevy-{sha[:12]}{tag}"


def build(sha, feats, out):
    CACHE.mkdir(exist_ok=True)
    wt = CACHE / f"wt-{sha[:12]}"
    if wt.exists():
        subprocess.run(["git", "-C", str(REPO), "worktree", "remove", "--force", str(wt)],
                       capture_output=True)
    subprocess.run(["git", "-C", str(REPO), "worktree", "add", "-f", "--detach", str(wt), sha],
                   capture_output=True, check=True)
    cmd = ["cargo", "build", "-q", "--profile", "release-perf", "-p", "kevy", "--bin", "kevy"]
    if feats:
        cmd += ["--features", ",".join(feats)]
    env = dict(os.environ, CARGO_TARGET_DIR=str(CACHE / "target"))
    try:
        subprocess.run(cmd, cwd=wt, env=env, check=True)
        shutil.copy2(CACHE / "target" / "release-perf" / "kevy", out)
    finally:
        subprocess.run(["git", "-C", str(REPO), "worktree", "remove", "--force", str(wt)],
                       capture_output=True)


def binary(spec, build_missing=True):
    """(path, label) for a spec; builds a rev that is not cached yet."""
    if is_path(spec):
        return str(pathlib.Path(spec).resolve()), spec
    rev, feats = split_spec(spec)
    name, sha = resolve_rev(rev)
    out = cached(sha, feats)
    if not out.exists():
        if not build_missing:
            raise FileNotFoundError(f"{spec} is not built yet")
        print(f"perfgate: building {name} ({sha[:12]}){' +' + ','.join(feats) if feats else ''}",
              flush=True)
        build(sha, feats, out)
    label = f"{name} ({sha[:12]}{'+' + ','.join(feats) if feats else ''})"
    return str(out), label

#!/usr/bin/env bash
# Publish a built site as the `site` branch of origin: one orphan commit
# whose root is the site, replacing whatever the branch held.
#
#   DRY_RUN=false bash scripts/push-site-branch.sh web/dist 7.0.0
#
# The web server pulls that branch and serves it, so pushing it is the
# deploy. Anything but DRY_RUN=false only reports what would be pushed and
# pushes nothing, to no ref.
#
# By content, against what the branch already holds:
#   the same tree            already published, nothing pushed, exit 0
#   the same version, a different tree   exit 1 (one version, one site)
#   a newer version          exit 1 (a re-run never rolls the site back)
#   otherwise                push
set -euo pipefail

DIST=${1:?usage: push-site-branch.sh <dist-dir> <version>}
V=${2:?usage: push-site-branch.sh <dist-dir> <version>}
DRY_RUN=${DRY_RUN:-true}

version_of() { python3 -c 'import json,sys; print(json.load(sys.stdin).get("version",""))'; }

[ -f "$DIST/index.html" ] || { echo "✗ $DIST/index.html is missing: the branch root must be the site root" >&2; exit 1; }
got=$(version_of < "$DIST/build.json")
[ "$got" = "$V" ] || { echo "✗ $DIST/build.json says $got, not $V" >&2; exit 1; }

GIT_DIR_ABS=$(git rev-parse --absolute-git-dir)
INDEX=$(mktemp)
rm -f "$INDEX"
trap 'rm -f "$INDEX"' EXIT
# -f: the tree is the build output, whatever an ignore file says.
GIT_INDEX_FILE=$INDEX git -C "$DIST" --git-dir="$GIT_DIR_ABS" --work-tree=. add -A -f .
tree=$(GIT_INDEX_FILE=$INDEX git --git-dir="$GIT_DIR_ABS" write-tree)
files=$(git ls-tree -r "$tree" | wc -l | tr -d ' ')
echo "built site $V: tree $tree, $files files"

lease=""
rc=0
git ls-remote --exit-code --heads origin site >/dev/null || rc=$?
if [ "$rc" = 0 ]; then
    git fetch --quiet --depth 1 origin "+refs/heads/site:refs/remotes/origin/site"
    old=$(git rev-parse refs/remotes/origin/site)
    old_tree=$(git rev-parse "$old^{tree}")
    old_v=$(git show "$old:build.json" 2>/dev/null | version_of || true)
    lease=$old
    echo "the site branch holds ${old_v:-an unversioned site} (tree $old_tree)"
    if [ "$old_tree" = "$tree" ]; then
        echo "✓ already published site $V, content matches — nothing to push"
        exit 0
    fi
    if [ "$old_v" = "$V" ]; then
        echo "✗ the site branch already holds $V with different content" >&2
        exit 1
    fi
    if [ -n "$old_v" ] && [ "$(printf '%s\n%s\n' "$old_v" "$V" | sort -V | tail -1)" = "$old_v" ]; then
        echo "✗ the site branch holds $old_v, newer than $V; refusing to roll it back" >&2
        exit 1
    fi
elif [ "$rc" != 2 ]; then
    echo "✗ could not ask origin about the site branch (git ls-remote exit $rc)" >&2
    exit 1
else
    echo "origin has no site branch yet"
fi

if [ "$DRY_RUN" != "false" ]; then
    echo "dry run: would push an orphan commit of tree $tree ($files files) to refs/heads/site"
    exit 0
fi

commit=$(GIT_AUTHOR_NAME="github-actions[bot]" \
         GIT_AUTHOR_EMAIL="41898282+github-actions[bot]@users.noreply.github.com" \
         GIT_COMMITTER_NAME="github-actions[bot]" \
         GIT_COMMITTER_EMAIL="41898282+github-actions[bot]@users.noreply.github.com" \
         git commit-tree "$tree" -m "site $V")
# The lease makes the replacement conditional on the branch still being
# what was compared above (or still absent).
git push --force-with-lease="refs/heads/site:$lease" origin "$commit:refs/heads/site"
echo "✓ pushed site $V as $commit"

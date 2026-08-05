#!/usr/bin/env bash
# rootgate — the repo root is not a data directory, gated.
#
# .claude/rule/hygiene.md's first rule is "never run a server or an
# embedded store in the repo root". It has now been broken twice, and both
# times nothing noticed, because the two ways it shows up are each
# invisible to the tool you would expect to catch them:
#
#   - Artifacts that ARE gitignored (aof-*, dump-*, shards.meta) never
#     appear in `git status`. 1993 of them accumulated once; 1094 more by
#     2026-07-23. The safety net is what hides the mess.
#   - Artifacts that are NOT gitignored get swept up by a `git add -A` and
#     become tracked files. Thirty-two feed-*.gen/.meta rode into the repo
#     that way under db39ee11, a commit about ZSET ranking.
#
# So this gate asks the filesystem and the index directly, rather than
# asking git whether it feels dirty.
#
#   bash bench/rootgate.sh
#
# Run it after the test suite: check 2 only means something once something
# has had the chance to write. In CI it follows the workspace test step,
# which is what makes "tests must not write to cwd" mechanically true
# instead of merely written down.
set -uo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
cd "$HERE" || exit 1
fail=0

# Every runtime file kevy writes into a store directory. Keep in step with
# the writers: kevy-persist's aof/rdb/shards/feed_meta, kevy-index's
# catalog. A shape missing here is a shape this gate cannot see.
shapes='aof-*.aof* dump-*.rdb* shards.meta feed-*.gen feed-*.meta index-catalog.meta'

# The same, for writers that make a DIRECTORY rather than a file: the
# cold tier (data_dir/tier/<shard>/vlog-*.dat) and windowed segments
# (data_dir/segs-<shard>/). Both arrived after this gate did, and neither
# was added to the list above -- so on 2026-08-06 a server started in the
# repo root without --dir left tier/0..15 sitting here and this gate said
# PASS. The list is only as good as the last writer added to it.
#
# Checked by existence, not by contents: an empty tier/ is still a store
# that opened here, and `ls -A` on an empty directory says nothing at all.
dir_shapes='tier segs-*'

# 1. No artifact may be TRACKED, wherever it sits. This is the one that
#    survives a fresh clone, so it is the one that keeps the accident from
#    being re-committed by someone else's `git add -A`.
tracked=$(git ls-files | grep -E '(^|/)(aof-[0-9]+\.aof|dump-[0-9]+\.rdb|shards\.meta|feed-[0-9]+\.(gen|meta)|index-catalog\.meta|tier/[0-9]+/|segs-[0-9]+/)' || true)
if [ -n "$tracked" ]; then
    echo "rootgate: FAIL — runtime artifacts are tracked in git:"
    echo "$tracked" | sed 's/^/  /'
    echo "  these are written by a running store, not source. Untrack them"
    echo "  (git rm --cached) and add the shape to .gitignore."
    fail=1
fi

# 2. The repo root must hold none of them on disk. Gitignored or not.
# shellcheck disable=SC2086
present=$(ls -A $shapes 2>/dev/null || true)
if [ -n "$present" ]; then
    n=$(echo "$present" | wc -l | tr -d ' ')
    echo "rootgate: FAIL — $n runtime artifact(s) in the repo root:"
    echo "$present" | head -8 | sed 's/^/  /'
    [ "$n" -gt 8 ] && echo "  … and $((n - 8)) more"
    echo "  something opened a store here. Data directories belong in a"
    echo "  mktemp dir or a bench script's \$DIR — see .claude/rule/hygiene.md."
    fail=1
fi

# 3. Same for the directory-shaped writers.
for d in $dir_shapes; do
    for hit in $d; do
        [ -e "$hit" ] || continue
        echo "rootgate: FAIL — runtime store directory in the repo root: $hit"
        echo "  a store opened here (the tier and segment writers use"
        echo "  data_dir, which defaults to '.'). Pass --dir, or start the"
        echo "  server somewhere else — see .claude/rule/hygiene.md."
        fail=1
    done
done

[ "$fail" -eq 0 ] && echo "rootgate: PASS — repo root clean, no artifacts tracked"
exit "$fail"

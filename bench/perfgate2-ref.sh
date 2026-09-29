# shellcheck shell=bash
# Sourced by perfgate2.sh after perfgate-ref.sh: which commit each side is,
# and the mutant builds of the judging-power proof.

# The rolling reference answers "did this change regress": the point the
# candidate branched from develop. On develop itself that point is the
# candidate, so the reference is the last commit that passed, recorded in the
# baseline when it passed.
rolling_ref() {
  local mb head recorded
  head=$(git -C "$REPO" rev-parse HEAD)
  mb=$(git -C "$REPO" merge-base HEAD origin/develop 2>/dev/null) \
    || { echo "perfgate2: no merge-base with origin/develop (fetch it first?)" >&2; return 0; }
  if [ "$mb" != "$head" ]; then printf "%s" "$mb"; return 0; fi
  recorded=$(python3 -c "import json;print(json.load(open('$BASELINE')).get('rolling_ref') or '')")
  [ -n "$recorded" ] \
    || echo "perfgate2: HEAD is on develop and the baseline records no last-passing commit" >&2
  printf "%s" "$recorded"
}

# The release anchor answers "how far has the cost drifted since the last
# release", judged on the C lines only: they are the ones stable across months.
anchor_tag() {
  git -C "$REPO" describe --tags --abbrev=0 --match 'v[0-9]*' HEAD 2>/dev/null
}

# Mutations of the judging-power proof. Each is a patch applied to a copy of
# HEAD in the reference cache and built there; the product tree carries no
# switch for any of them. M1b is M1 at half the extra instructions, M4 is no
# build at all: the candidate is the same binary given one core fewer.
mutant_patch() {
  case $1 in
    M1|M1b) echo spin ;;
    M2) echo sleep ;;
    M3) echo miss ;;
    M5) echo syscall ;;
    M4) echo "" ;;
    *) return 1 ;;
  esac
}

mutant_binary() { # $1 = sha, $2 = patch name
  local sha=$1 patch="$HERE/mutants/$2.patch"
  local tag
  tag=$(git -C "$REPO" hash-object "$patch" | cut -c1-8)
  local cache="$HERE/.perfgate-ref" out="$HERE/.perfgate-ref/kevy-${sha:0:12}-$2-$tag"
  [ -x "$out" ] && { printf "%s" "$out"; return 0; }
  mkdir -p "$cache"
  local wt="$cache/wt-${sha:0:12}-$2"
  git -C "$REPO" worktree add -f --detach "$wt" "$sha" >/dev/null 2>&1 \
    || { echo "perfgate2: cannot check out $sha for mutant $2" >&2; return 0; }
  if ! git -C "$wt" apply "$patch" \
    || ! ( cd "$wt" && cargo build -q --profile release-perf -p kevy --bin kevy ); then
    echo "perfgate2: mutant $2 does not apply or build at ${sha:0:12} — regenerate the patch" >&2
    git -C "$REPO" worktree remove --force "$wt" >/dev/null 2>&1
    return 0
  fi
  cp "$wt/target/release-perf/kevy" "$out"
  git -C "$REPO" worktree remove --force "$wt" >/dev/null 2>&1
  printf "%s" "$out"
}

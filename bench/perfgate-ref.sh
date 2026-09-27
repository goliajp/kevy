# Sourced by perfgate.sh.

# ---------- the reference binary ----------
# The gate is RELATIVE: the candidate is compared against a binary built from
# the baseline's own commit, measured on this box in this session. Comparing
# today's number against a number recorded weeks ago cannot work here — the
# box drifts (2026-07-12: the SAME code measured 24.3M when the baseline was
# taken and 21.0M three weeks later, a 13% slide that dwarfs the 8% gate).
# Interleaving the two binaries makes that drift cancel instead of deciding
# the verdict.
ref_binary() {
  # Two statements, not one: `local` expands ALL its arguments before it
  # assigns any of them, so an `out=...${sha}` sharing the line with
  # `sha=$1` reads sha BEFORE it exists — under `set -u` that aborts the
  # gate ("sha: unbound variable"), which is exactly what it did.
  local sha=$1
  local cache="$HERE/.perfgate-ref" out="$HERE/.perfgate-ref/kevy-${sha:0:12}"
  [ -x "$out" ] && { printf "%s" "$out"; return 0; }
  mkdir -p "$cache"
  local wt="$cache/wt-${sha:0:12}"
  git -C "$HERE/.." worktree add -f --detach "$wt" "$sha" >/dev/null 2>&1 \
    || refuse "cannot check out the baseline commit $sha (fetch it first?)"
  ( cd "$wt" && cargo build -q --profile release-perf -p kevy --bin kevy ) \
    || { git -C "$HERE/.." worktree remove --force "$wt" >/dev/null 2>&1; refuse "reference build failed at $sha"; }
  cp "$wt/target/release-perf/kevy" "$out"
  git -C "$HERE/.." worktree remove --force "$wt" >/dev/null 2>&1
  printf "%s" "$out"
}

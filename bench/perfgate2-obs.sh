# shellcheck shell=bash
# Sourced by perfgate2.sh: one observation, and the paired sequence of them.

judge() { python3 "$HERE/perfgate2_judge.py" "$@"; }

# One window of the running angle. Echoes the window's JSON; returns the
# collector's code (0 keep, 10 discard, anything else a refusal).
one_window() { # $1 = side, $2 = obs, $3 = win, $4 = server cpus
  local lat=()
  [ -z "$GENS" ] && lat=(-- taskset -c 5 python3 "$HERE/perfgate_hybrid.py" run "$PORT" "$N_HYBRID")
  python3 "$HERE/perfgate2_window.py" --baseline "$BASELINE" --angle "$ANGLE" \
    --side "$1" --obs "$2" --win "$3" --srv-pid "$SRV" --srv-cpus "$4" \
    --port "$PORT" --secs "$WINDOW" --gens "$GENS" --lines "$LINES" "${lat[@]}"
}

# One observation: a fresh server, warmed, loaded, and WINDOWS kept windows.
# A window broken by the box is retaken; K in a row is a refusal, because a
# gate that keeps retaking is measuring the neighbours, not the engine.
observe() { # $1 = side, $2 = obs, $3 = binary, $4 = server cpus
  local kept=0 win=0 streak=0 out rc
  angle_up "$3" "$4" "$ANGLE"
  [ -n "$GENS" ] && sleep "$RAMP"
  while [ "$kept" -lt "$WINDOWS" ]; do
    win=$((win + 1))
    out=$(one_window "$1" "$2" "$win" "$4")
    rc=$?
    case $rc in
      0) echo "$out" >>"$SAMPLES"; kept=$((kept + 1)); streak=0 ;;
      10) streak=$((streak + 1))
          if [ "$streak" -ge "$MAX_DISCARDS" ]; then
            angle_down
            refuse "$ANGLE: $MAX_DISCARDS windows in a row discarded (last reason above) — \
not a verdict about the engine$(echo "$out" | grep -q client-bound && echo '; CLIENT-BOUND')"
          fi ;;
      *) angle_down; exit 2 ;;
    esac
  done
  angle_down
}

# The candidate side's server cpus and mutant knob for this observation.
cand_setup() {
  CAND_CPUS=$SRV_CPUS
  unset KEVY_MUTANT_SPIN
  # the latency angle has no instruction count to size M1 from
  [[ $ANGLE == *_us ]] && [[ ${MUTANT:-} == M1* ]] && return 0
  case ${MUTANT:-} in
    M4) CAND_CPUS=0-2 ;;
    M1) KEVY_MUTANT_SPIN=$(judge spin --samples "$SAMPLES" --angle "$ANGLE" --frac 0.03) || exit 2 ;;
    M1b) KEVY_MUTANT_SPIN=$(judge spin --samples "$SAMPLES" --angle "$ANGLE" --frac 0.015) || exit 2 ;;
  esac
  [ -n "${KEVY_MUTANT_SPIN:-}" ] && export KEVY_MUTANT_SPIN
  return 0
}

observe_side() { # $1 = side, $2 = obs
  if [ "$1" = ref ]; then
    unset KEVY_MUTANT_SPIN
    observe ref "$2" "$REF_BIN" "$SRV_CPUS"
  else
    cand_setup
    observe cand "$2" "$CAND_BIN" "$CAND_CPUS"
    unset KEVY_MUTANT_SPIN
  fi
}

# Pairs in ABBA order until every line of the angle is decided, or a fixed
# number of pairs (calibration). Reference first on odd pairs, so the M1
# mutant always has a reference count to size itself from.
pairs_for_angle() { # $1 = fixed pair count, or empty for sequential
  local k=0 next
  while :; do
    if [ -n "$1" ]; then
      [ "$k" -lt "$1" ] || break
    else
      next=$(judge next --samples "$SAMPLES" --baseline "$BASELINE" \
        --fingerprint "$FP" --angle "$ANGLE" --lines "$JUDGE_LINES") || exit 2
      [ "$next" = more ] || break
    fi
    k=$((k + 1))
    if [ $((k % 2)) -eq 1 ]; then
      observe_side ref "$k"; observe_side cand "$k"
    else
      observe_side cand "$k"; observe_side ref "$k"
    fi
    echo "perfgate2: $ANGLE pair $k done" >&2
  done
}

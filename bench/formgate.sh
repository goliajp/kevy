#!/usr/bin/env bash
# formgate — every hash verb must be exercised against the packed row.
#
# A hash has two storage forms and one wire contract. Every verb picks
# what to do by matching on the form, and every one of those matches ends
# in a catch-all, so a verb that does not name the packed form is not a
# compile error: it is a WRONGTYPE at runtime about a row the server is
# holding, or a silently wrong answer.
#
# That defect landed three times in two days — reads, then the tiering
# and memory paths, then HDEL and HINCRBYFLOAT — and each time it was
# found by reading the code looking for `Value::Hash`, which found a
# different subset every time. This is that search done mechanically:
# enumerate the public hash verbs, and require each one to appear in the
# parity suite. It cannot prove a verb is CORRECT on a packed row; it
# proves nobody added one without deciding.
#
# Adding a verb to the exemption list is a decision to be argued in the
# diff, which is the point — the silent version is what this replaces.
set -euo pipefail
cd "$(dirname "$0")/.."

PARITY=crates/kevy-store/tests/packed_row_parity.rs
SRC=(crates/kevy-store/src/hash.rs crates/kevy-store/src/hash_read.rs
     crates/kevy-store/src/hash_ttl.rs)

for f in "$PARITY" "${SRC[@]}"; do
  [ -f "$f" ] || { echo "formgate: FAIL — $f is missing; this gate checks nothing"; exit 1; }
done

# Verbs whose behaviour on a packed row is covered elsewhere:
#   hexpire_at  tests_tier::a_packed_row_answers_the_field_ttl_precheck
#   hpttl       reads the field-TTL sidecar, never the row
#   hpersist    reads the field-TTL sidecar, never the row
# A flat list, not an associative array: this runs on the box's bash and
# on a Mac's bash 3.2, and the second one has no associative arrays.
EXEMPT="hexpire_at hpttl hpersist"

verbs=$(grep -ho "    pub fn h[a-z_]*(" "${SRC[@]}" | sed 's/.*pub fn \(h[a-z_]*\)(.*/\1/' | sort -u)
[ -n "$verbs" ] || { echo "formgate: FAIL — found no hash verbs; the search is broken"; exit 1; }

n=0; missing=()
for v in $verbs; do
  n=$((n + 1))
  if grep -q "\.$v(" "$PARITY"; then continue; fi
  case " $EXEMPT " in *" $v "*) continue ;; esac
  missing+=("$v")
done

[ "$n" -ge 10 ] || { echo "formgate: FAIL — only $n verbs found; the pattern stopped matching"; exit 1; }

if [ ${#missing[@]} -gt 0 ]; then
  echo "formgate: FAIL — these hash verbs are never called against a packed row:"
  printf '  %s\n' "${missing[@]}"
  echo "Add a case to $PARITY, or an entry to this gate's exemption list saying where it is covered."
  exit 1
fi

ex=$(echo "$EXEMPT" | wc -w | tr -d " ")
echo "formgate: PASS — $n hash verbs, $ex covered elsewhere, the rest exercised against the packed row"

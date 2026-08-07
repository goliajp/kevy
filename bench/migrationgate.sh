#!/usr/bin/env bash
# migrationgate — migration day, end to end, repeatable (V2 train).
#
# The whole chain against a REAL PostgreSQL:
#   seed → pg_dump → sql plan (billing named-dropped, 3 declarable)
#        → day-2 schema compiled + applied → COPY csv → RESP frames
#        → import → row counts + sampled fields reconciled
#        → TABLE.VERIFY drift 0 → doctor.
#
# Runs on a box with docker + psql-in-container + redis-cli. Every
# resource is trap-cleaned: the PG container, the kevy server, the
# work dir. Deterministic: the seed is generate_series only.
set -uo pipefail
cd "$(dirname "$0")/.."

PGC=kevy-migrationgate-pg
PGPORT=15441
KPORT=6310
WORK=$(mktemp -d "${TMPDIR:-/tmp}/miggate-XXXXXX")
KEVY_PID=""
fail() { echo "migrationgate: FAIL — $1"; exit 1; }
cleanup() {
    if [ -n "$KEVY_PID" ]; then
        kill -9 "$KEVY_PID" 2>/dev/null
        wait "$KEVY_PID" 2>/dev/null
    fi
    docker rm -f "$PGC" >/dev/null 2>&1
    rm -rf "$WORK"
}
trap cleanup EXIT

PSQL="docker exec -i $PGC psql -U postgres -q"

# ── 1. seed a real PG ──
docker rm -f "$PGC" >/dev/null 2>&1
docker run -d --name "$PGC" -e POSTGRES_PASSWORD=drill \
    -p "127.0.0.1:$PGPORT:5432" postgres:18-bookworm >/dev/null || fail "docker run"
for _ in $(seq 60); do
    docker exec "$PGC" pg_isready -U postgres -q && break
    sleep 1
done
$PSQL -f - < bench/migration-corpus/seed-schema.sql || fail "seed schema"
$PSQL -f - < bench/migration-corpus/seed-data.sql || fail "seed data"

# ── 2. dump + plan: the refusal must be NAMED, the rest declarable ──
docker exec "$PGC" pg_dump -U postgres --schema-only postgres > "$WORK/dump.sql" \
    || fail "pg_dump"
PLAN=$(target/release/kevy-cli sql plan "$WORK/dump.sql") || fail "sql plan errored"
echo "$PLAN" | grep -q "billing: type 'money'" || fail "billing must drop by name"
[ "$(echo "$PLAN" | grep -cE '^  (users|threads|messages)$')" = 3 ] \
    || fail "3 tables must stay declarable"

# ── 3. day-2 schema applies to a fresh kevy ──
target/release/kevy --port $KPORT --threads 2 --dir "$WORK/kevy" &>"$WORK/kevy.log" &
KEVY_PID=$!
for _ in $(seq 50); do
    redis-cli -p $KPORT ping 2>/dev/null | grep -q PONG && break
    sleep 0.2
done
target/release/kevy-cli sql compile bench/migration-corpus/day2-schema.sql \
    --apply --url 127.0.0.1:$KPORT >/dev/null || fail "day-2 schema apply"

# ── 4. the data leg: COPY csv → RESP frames → import ──
# The gate plays the app: it KNOWS the authoritative record (the
# lesson says a tool must not guess), so the billing mapping happens
# here, in SQL, where the operator's knowledge lives.
$PSQL -Atc "COPY (SELECT id,email,name,created_at,flags FROM users ORDER BY id) TO STDOUT WITH CSV" > "$WORK/users.csv"
$PSQL -Atc "COPY (SELECT tid,owner_id,subject,updated_at,msg_count FROM threads ORDER BY tid) TO STDOUT WITH CSV" > "$WORK/threads.csv"
$PSQL -Atc "COPY (SELECT mid,tid,author_id,sent_at,body,spam_score FROM messages ORDER BY mid) TO STDOUT WITH CSV" > "$WORK/messages.csv"
$PSQL -Atc "COPY (SELECT id,user_id,(amount::numeric*100)::bigint,host(src_ip) FROM billing ORDER BY id) TO STDOUT WITH CSV" > "$WORK/billing.csv"

python3 - "$WORK" <<'EOF' || fail "frame conversion"
import csv, sys
work = sys.argv[1]
tables = {
    "users":    ["id", "email", "name", "created_at", "flags"],
    "threads":  ["tid", "owner_id", "subject", "updated_at", "msg_count"],
    "messages": ["mid", "tid", "author_id", "sent_at", "body", "spam_score"],
    "billing":  ["id", "user_id", "amount_cents", "src_ip"],
}
def resp(args):
    out = [f"*{len(args)}\r\n".encode()]
    for a in args:
        b = a.encode()
        out.append(f"${len(b)}\r\n".encode() + b + b"\r\n")
    return b"".join(out)
with open(f"{work}/frames.resp", "wb") as f:
    for table, cols in tables.items():
        with open(f"{work}/{table}.csv", newline="") as c:
            for row in csv.reader(c):
                key = f"{table}:{row[0]}"
                args = ["HSET", key]
                for name, val in zip(cols, row):
                    args += [name, val]
                f.write(resp(args))
EOF
redis-cli -p $KPORT --pipe < "$WORK/frames.resp" | grep -q "errors: 0" \
    || fail "import reported errors"

# ── 5. reconcile: counts, then sampled fields ──
for t in users:2000 threads:10000 messages:40000 billing:500; do
    name=${t%%:*}; want=${t##*:}
    got=$(redis-cli -p $KPORT --scan --pattern "$name:*" | wc -l | tr -d ' ')
    [ "$got" = "$want" ] || fail "$name row count: kevy $got vs pg $want"
done
for probe in "users:1500:email" "threads:7777:subject" "messages:39999:body"; do
    IFS=: read -r t k f <<< "$probe"
    pgcol=$f
    pk=$([ "$t" = users ] && echo id || { [ "$t" = threads ] && echo tid || echo mid; })
    want=$($PSQL -Atc "SELECT $pgcol FROM $t WHERE $pk = $k")
    got=$(redis-cli -p $KPORT HGET "$t:$k" "$f")
    [ "$got" = "$want" ] || fail "$t:$k.$f: kevy '$got' vs pg '$want'"
done

# ── 6. verify + doctor ──
for t in users threads messages; do
    V=$(redis-cli -p $KPORT TABLE.VERIFY "$t")
    echo "$V" | grep -q "drift" || fail "TABLE.VERIFY $t answered nothing"
    # every drift counter must be zero
    if echo "$V" | awk '/^drift$/{getline; if ($0+0 != 0) exit 1}' ; then :; else
        fail "TABLE.VERIFY $t reports drift"
    fi
done
target/release/kevy-cli doctor -p $KPORT >/dev/null || fail "doctor"

echo "migrationgate: PASS — dump planned (billing named-dropped), day-2 schema applied, 52500 rows moved and reconciled, verify drift 0, doctor green"

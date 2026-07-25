#!/usr/bin/env bash
# Decompose the IDX.QUERY gap: is it the 16-way fan-out?
#
# No code change needed — vary the shard count and watch. If fan-out
# dominates, idx latency tracks the shard count. `pk` is the control:
# it routes to exactly one shard whatever the count, so it should stay
# flat. Anything the two lines do NOT share is fan-out.
set -uo pipefail
cd ~/kevy
CSV=$HOME/fanout/data.csv
mkdir -p ~/fanout
[ -f "$CSV" ] || ~/pgbench-venv/bin/python bench/pgcompare.py gen --rows 500000 --out "$CSV" --pad 400
echo "shards | pk p50 | pk p99 | idx p50 | idx p99 | page p50 | page p99"
for S in 1 2 4 8 16; do
  D=~/fanout/s$S; rm -rf "$D"; mkdir -p "$D"
  KEVY_BIND=127.0.0.1 ./target/release/kevy --port 6397 --threads "$S" --dir "$D" --no-aof > "$D/srv.log" 2>&1 &
  for _ in $(seq 40); do
    ~/pgbench-venv/bin/python -c "import socket;socket.create_connection(('127.0.0.1',6397),0.3).close()" 2>/dev/null && break
    sleep 0.5
  done
  OUT=$(~/pgbench-venv/bin/python bench/pgcompare.py kevy --csv "$CSV" --port 6397 --mode "s$S" --datadir "$D" --samples 2000 2>/dev/null | tail -1)
  ~/pgbench-venv/bin/python - "$OUT" <<'PY'
import json,sys
d=json.loads(sys.argv[1])
print(f"  {d['mode'][1:]:>4} | {d['pk_p50_us']:>6} | {d['pk_p99_us']:>6} | {d['idx_p50_us']:>7} | {d['idx_p99_us']:>7} | {d['page_p50_us']:>8} | {d['page_p99_us']:>8}")
PY
  pkill -f "kevy --port 6397" 2>/dev/null; sleep 1
done
rm -rf ~/fanout/s*
echo DONE

# kevy-uring cross-language perf bench

Workload: `nop_round_trip` — one `prep_nop` → `submit_and_wait(1)` → reap
cycle, 100 000 iterations per sample, 25 samples per run. Measures the
kernel-floor cost of io_uring (the `io_uring_enter` syscall + cursor
advance). Any thin wrapper over the raw ABI should land at this floor.

## Competitors

- **kevy-uring** (rust) — the stone, zero deps over the kernel ABI
- **liburing** (c) — Jens Axboe's reference implementation (the "what
  most C/C++ io_uring code uses" yardstick)

Async runtime layers (`tokio-uring`, `monoio`) and bindgen wrappers
(`io-uring` crate) are excluded from this bench: their per-call cost is
the kernel floor PLUS their respective task / scheduler overhead, which
is not the same metric. The relevant cross-language comparison is "raw
engine vs raw engine".

## Reproducibility (Linux only)

```bash
# Requires: liburing-dev (Debian: apt-get install liburing-dev) + cargo
cd perfs/comparative/kevy-uring
bash run.sh > results-$(date -I).jsonl
# Multirun (5x):
rm -f multirun.jsonl
for i in 1 2 3 4 5; do bash run.sh >> multirun.jsonl; done
jq -s 'group_by(.competitor) | map({c:.[0].competitor, medians:[.[].value_median], min_med:([.[].value_median] | min)})' multirun.jsonl
```

## Headline (2026-05-27, lx64 metal, 5-run min-of-medians)

| competitor | ns/op (min-of-medians) | verdict |
|---|---:|---|
| **kevy-uring** | **148** | ✅ at kernel floor; 4 ns ahead of liburing |
| liburing | 152 | reference (Jens Axboe) |

# Cross-language stone bench harness

Per [[feedback-mailrs-stone-deep-polish-method]], each stone must beat
`max(Rust, Go, C, C++ competitors)` on its primary perf metric before it
can be published. This directory holds the comparative benches that
prove (or disprove) that gate per stone.

## Layout

```
perfs/comparative/
├── README.md              (this file — methodology + JSON schema)
├── _template/             (copy this for each new stone Phase P)
│   ├── rust/              (Rust competitors crate + kevy stone)
│   │   ├── Cargo.toml
│   │   └── src/main.rs    (per-bench main; prints JSON line per metric)
│   ├── go/                (Go competitor program)
│   │   ├── go.mod
│   │   └── main.go        (prints JSON line per metric)
│   ├── c/                 (C competitor program)
│   │   ├── Makefile
│   │   └── main.c         (prints JSON line per metric)
│   ├── cpp/               (C++ competitor program)
│   │   ├── Makefile
│   │   └── main.cpp       (prints JSON line per metric)
│   ├── run.sh             (drives every language; emits jsonl)
│   └── README.md          (lists competitors + primary metric)
└── <stone>/               (per-stone, populated during Phase P{n})
    ├── …
    └── results-<date>.jsonl  (one JSON line per (language, competitor, metric))
```

## JSON record schema (one line per measurement)

```json
{
  "stone": "kevy-hash",
  "language": "rust",
  "competitor": "ahash",
  "workload": "hash_u64",
  "metric": "ns_per_op",
  "value_median": 3.2,
  "value_p95": 4.1,
  "value_min": 2.9,
  "iterations": 1000000,
  "host": "M4-Pro-aarch64",
  "date": "2026-05-27T06:30:00+09:00",
  "version": "0.4.0"
}
```

Stone gate (per metric per workload):
```
median(kevy-stone) ≤ min(median(all competitors)) for "lower-is-better" metrics (ns/op, alloc count)
median(kevy-stone) ≥ max(median(all competitors)) for "higher-is-better" metrics (MB/s, ops/sec)
```

## Why JSON lines

- Each language program is responsible for printing one JSON object per
  metric to stdout.
- `run.sh` concatenates them into a single `.jsonl` file.
- Diffing across stone versions is a `jq` query, not a regex on prose.
- Comparing against `max(competitors)` is a pure transform on the file.

## Adding a new stone (Phase P{n} kickoff checklist)

1. `cp -r _template/ <stone-name>/`
2. Edit `<stone-name>/README.md` — list competitors per language + the
   stone's primary metric
3. Implement each language entry point — same workload, same iteration
   count, emit the JSON schema above
4. Run `bash <stone-name>/run.sh > <stone-name>/results-$(date +%F).jsonl`
5. Verify gate (`jq` query in `<stone-name>/README.md`)
6. Commit results before any deep-polish change

## Hardware

Comparative benches must run on **the same machine** for all
languages within one comparison run. Cross-host comparison is invalid.
Each `results-<date>.jsonl` records `"host"` so consumers can drop
stale-host rows.

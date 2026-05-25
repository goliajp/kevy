# kevy performance audit

Living workspace for performance work. Not shipped. Modelled on
`mailrs/perfs/` — adapted for a systems server with a measurement constraint.

## Layout

```
perfs/
├── README.md     ← you are here (mode of operation)
├── TREE.md       ← component map: every crate's hot paths → status → topic links
├── topics/       ← one file per investigation
│   ├── _template.md
│   └── NN-slug.md
└── data/         ← raw measurements, dated, append-only
    └── YYYY-MM-DD/
```

## The measurement constraint (why this looks different from mailrs)

mailrs measures a live web service end-to-end. kevy's headline numbers are
**server throughput** (`-c1` / `-c50` / pub-sub vs valkey), but the dev host is
permanently loaded by other projects, and kevy busy-polls — so a contended host
starves its reactor and full-system numbers are unreproducible. See `bench/` for
the macro-benchmark harness; it only yields clean numbers on an idle host.

So the **primary tool here is component micro-benchmarks**, not full-system runs:

- Each perf-critical crate has `examples/bench_*.rs` (exploration, A/B with
  ratios) and `tests/perf_gate.rs` (regression gate), both built on the
  zero-dep `kevy-bench` harness.
- Variants run **back-to-back in one process**, so the **ratio between them
  holds even on a loaded host** (absolute ns drift, ratio doesn't). Treat
  `median_ns` as relative unless the host is known idle.

This is also what makes the std-self-host evaluation possible without a clean
host: we compare a candidate against the incumbent under identical conditions.

## Mode of operation

1. **Establish the map.** `TREE.md` lists every crate's hot paths, each marked
   ✓ healthy / · informational / ⚠ (links to a topic). New surface area goes
   into TREE before any deep-dive.
2. **Open a topic per anomaly.** Anything ⚠ gets `topics/NN-slug.md` (copy
   `_template.md`, next free number, status `open`). The topic owns the
   hypotheses, investigation log, decision, and post-fix verification numbers.
3. **Measure first, then theorize.** Numbers land in `data/<date>/` *before*
   opinions form. Hypotheses without evidence are marked as such.
4. **Re-measure to close.** Closing a topic needs a fresh `data/<date>/` run
   showing the metric moved. Ladder: `open → investigating → fix proposed →
   fixed (vX.Y.Z)`.
5. **Data is append-only.** Never overwrite a `data/<date>/`; make a new dated
   one. Topic files may be rewritten freely.

## Conventions

- Headline figures are the **median** of the kevy-bench sampling, cross-checked
  over **≥3 process runs** for paths sensitive to host noise (e.g. populated-map
  lookups). Pure-hasher figures are stable in one run.
- Record host load (`sysctl -n vm.loadavg`) when it matters; ratios are robust
  to it, absolutes are not.
- `topics/` numbering is monotonic and never reused.

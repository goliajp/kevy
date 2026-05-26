# &lt;stone-name&gt; — cross-language comparative bench

Replace this README per stone when `cp _template/` is run. Fill in:

## Stone identity

- name: `<stone>`
- primary metric: `<metric>` — `lower-is-better` or `higher-is-better`
- workloads: `<workload-1>`, `<workload-2>`, ...

## Competitors

| language | competitor | crate / package | workload(s) |
|---|---|---|---|
| rust   | … | … | … |
| go     | … | … | … |
| c      | … | … | … |
| cpp    | … | … | … |

## Gate

```
median(kevy-stone) <op> max(all competitors' medians)
```

Pass criterion: kevy-stone strictly beats `max(competitors)` on the
primary metric (or, for `lower-is-better`, has strictly lower median).

## How to run

```bash
cd perfs/comparative/<stone>/
bash run.sh > results-$(date +%F).jsonl
jq -s 'group_by(.workload) | map({workload: .[0].workload, ranked: (sort_by(.value_median) | map({competitor, language, value_median}))})' \
  results-$(date +%F).jsonl > ranked-$(date +%F).json
```

## Results history

| date       | kevy version | best competitor      | kevy median | competitor median | pass? |
|------------|--------------|----------------------|-------------|-------------------|-------|
| YYYY-MM-DD | v0.1.0       | …                    | …           | …                 | …     |

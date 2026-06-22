# v1.25 precision bench (n=1000000 × runs=10, 1.96σ CI95)

Host: lx64  Date: 2026-06-22T09:44:42Z
kevy: --threads 1, taskset 0  |  valkey: --io-threads 10, taskset 0-9
Client: taskset 10-13, redis-benchmark 8.8

| scenario  | op  | server  | mean(rps)    | std        | ci95(±)   | mean(2σ-filt) | filt n     |
|---|---|---|---:|---:|---:|---:|---:|
| c1-P1     | SET | kevy    |      93907.7 |     2875.7 |     1782.3 |      94691.8 |          9 |
| c1-P1     | SET | valkey  |      62188.6 |      762.4 |      472.5 |      62188.6 |         10 |
| c1-P1     | GET | kevy    |      96920.5 |     1327.9 |      823.0 |      97261.4 |          9 |
| c1-P1     | GET | valkey  |      65130.3 |      379.1 |      235.0 |      65026.6 |          9 |
| c50-P1    | SET | kevy    |     191312.9 |     1887.6 |     1170.0 |     191739.2 |          9 |
| c50-P1    | SET | valkey  |     189624.7 |     6537.0 |     4051.7 |     191625.7 |          9 |
| c50-P1    | GET | kevy    |     189872.8 |     8485.9 |     5259.6 |     192438.9 |          9 |
| c50-P1    | GET | valkey  |     190896.3 |     2560.2 |     1586.8 |     190896.3 |         10 |
| c50-P16   | SET | kevy    |    2592769.0 |    15227.8 |     9438.3 |    2592769.0 |         10 |
| c50-P16   | SET | valkey  |    1773686.4 |   209069.1 |   129582.4 |    1822416.7 |          9 |
| c50-P16   | GET | kevy    |    2673729.7 |    43744.8 |    27113.3 |    2673729.7 |         10 |
| c50-P16   | GET | valkey  |    2632244.4 |   150980.1 |    93578.4 |    2678349.9 |          9 |
| c100-P1   | SET | kevy    |     189881.6 |     2074.4 |     1285.7 |     189881.6 |         10 |
| c100-P1   | SET | valkey  |     188484.2 |     1534.7 |      951.2 |     188484.2 |         10 |
| c100-P1   | GET | kevy    |     188769.1 |     5443.4 |     3373.8 |     190018.4 |          9 |
| c100-P1   | GET | valkey  |     190269.7 |     2136.0 |     1323.9 |     190754.3 |          9 |

## Cross-server ratios (kevy mean_filt / valkey mean_filt)

(see above table)

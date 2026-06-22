# v1.25 precision bench — UDS (n=1000000 × runs=10, 1.96σ CI95)

Host: lx64  Date: 2026-06-22T10:33:39Z
Transport: Unix-domain stream socket (no TCP loopback)
kevy: --threads 1 + KEVY_UNIX_SOCKET=/tmp/kevy.sock, taskset 0
valkey: --unixsocket /tmp/valkey.sock --io-threads 10, taskset 0-9
Client: taskset 10-13, redis-benchmark 8.8 -s <sock>

| scenario  | op  | server  | mean(rps)    | std        | ci95(±)   | mean(2σ-filt) | filt n     |
|---|---|---|---:|---:|---:|---:|---:|
| c1-P1     | SET | kevy    |     165870.1 |     1858.2 |     1151.7 |     166285.9 |          9 |
| c1-P1     | SET | valkey  |      96259.7 |     1485.8 |      920.9 |      96259.7 |         10 |
| c1-P1     | GET | kevy    |     167979.4 |     1430.4 |      886.5 |     167979.4 |         10 |
| c1-P1     | GET | valkey  |     105677.6 |     1283.6 |      795.6 |     105677.6 |         10 |
| c50-P1    | SET | kevy    |     339123.0 |     3061.5 |     1897.5 |     339123.0 |         10 |
| c50-P1    | SET | valkey  |     334192.3 |     2948.0 |     1827.2 |     334192.3 |         10 |
| c50-P1    | GET | kevy    |     334774.2 |     8711.8 |     5399.6 |     337216.5 |          9 |
| c50-P1    | GET | valkey  |     331911.2 |     4712.9 |     2921.1 |     331911.2 |         10 |
| c50-P16   | SET | kevy    |    4047227.5 |   199670.5 |   123757.1 |    4109772.7 |          9 |
| c50-P16   | SET | valkey  |    1748855.2 |   181665.4 |   112597.4 |    1748855.2 |         10 |
| c50-P16   | GET | kevy    |    4333663.6 |    65549.7 |    40628.2 |    4350281.7 |          9 |
| c50-P16   | GET | valkey  |    3416376.6 |   228971.5 |   141918.0 |    3416376.6 |         10 |
| c100-P1   | SET | kevy    |     331035.9 |     2685.4 |     1664.4 |     331035.9 |         10 |
| c100-P1   | SET | valkey  |     325782.3 |     4659.3 |     2887.9 |     325782.3 |         10 |
| c100-P1   | GET | kevy    |     334878.1 |     3737.9 |     2316.7 |     334878.1 |         10 |
| c100-P1   | GET | valkey  |     322324.8 |    15907.5 |     9859.6 |     326997.6 |          9 |

## Cross-server ratios (kevy mean_filt / valkey mean_filt)

(see above table)

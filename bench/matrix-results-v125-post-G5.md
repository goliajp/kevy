# kevy / valkey / redis matrix bench (median of 3 runs) — 2026-06-22T00:31Z

Host lx64 (Intel i7-10700K Comet Lake, 16 cores, mitigations=off).

```
scenario	server	op1	op2
c1-P1	kevys	95026.92	98007.19
c50-P1	kevys	195618.16	195045.84
c50-P16	kevys	2680965.25	2659574.50
c100-P1	kevys	193610.84	192270.70
c50-INCR	kevys	196540.89
c50-MSET	kevys	0
c50-10KB	kevys	156617.08	159109.00
c1-P1	redis	42378.87	43572.98
c50-P1	redis	167954.31	165234.62
c50-P16	redis	2493765.75	2506265.75
c100-P1	redis	167224.08	170794.19
c50-INCR	redis	148478.09
c50-MSET	redis	0
c50-10KB	redis	123839.01	124610.59
c1-P1	valkey	64446.83	65473.59
c50-P1	valkey	190585.09	189609.41
c50-P16	valkey	1926782.25	2439024.50
c100-P1	valkey	188111.36	188359.39
c50-INCR	valkey	188679.23
c50-MSET	valkey	0
c50-10KB	valkey	151515.14	153727.91
```

## Pivoted (RPS, kevy% of best-competitor)

| scenario | op | kevys | valkey | redis | kevy / best-competitor |
|---|---|---|---|---|---|
| c1-P1 | SET | 95027 | 64447 | 42379 | 147% ✅ ≥120% |
| c1-P1 | GET | 98007 | 65474 | 43573 | 150% ✅ ≥120% |
| c50-P1 | SET | 195618 | 190585 | 167954 | 103% ⚠ win<120% |
| c50-P1 | GET | 195046 | 189609 | 165235 | 103% ⚠ win<120% |
| c50-P16 | SET | 2680965 | 1926782 | 2493766 | 108% ⚠ win<120% |
| c50-P16 | GET | 2659574 | 2439024 | 2506266 | 106% ⚠ win<120% |
| c100-P1 | SET | 193611 | 188111 | 167224 | 103% ⚠ win<120% |
| c100-P1 | GET | 192271 | 188359 | 170794 | 102% ⚠ win<120% |
| c50-10KB | SET | 156617 | 151515 | 123839 | 103% ⚠ win<120% |
| c50-10KB | GET | 159109 | 153728 | 124611 | 104% ⚠ win<120% |

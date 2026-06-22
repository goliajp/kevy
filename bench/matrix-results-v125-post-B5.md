# kevy / valkey / redis matrix bench (median of 3 runs) — 2026-06-22T06:51Z

Host lx64 (Intel i7-10700K Comet Lake, 16 cores, mitigations=off).

```
scenario	server	op1	op2
c1-P1	kevys	96092.25	97560.98
c50-P1	kevys	192938.45	192270.70
c50-P16	kevys	2577319.50	2617801.00
c100-P1	kevys	188005.27	187441.42
c50-INCR	kevys	193498.44
c50-MSET	kevys	0
c50-10KB	kevys	152555.30	153846.16
c1-P1	redis	44195.64	45468.32
c50-P1	redis	151034.59	156519.02
c50-P16	redis	2183406.00	2053388.12
c100-P1	redis	150489.09	154583.41
c50-INCR	redis	158906.72
c50-MSET	redis	0
c50-10KB	redis	128949.06	124533.01
c1-P1	valkey	65919.58	67888.66
c50-P1	valkey	189789.33	190585.09
c50-P16	valkey	1883239.12	2512562.75
c100-P1	valkey	188893.08	187863.98
c50-INCR	valkey	189250.58
c50-MSET	valkey	0
c50-10KB	valkey	149476.83	152905.20
```

## Pivoted (RPS, kevy% of best-competitor)

| scenario | op | kevys | valkey | redis | kevy / best-competitor |
|---|---|---|---|---|---|
| c1-P1 | SET | 96092 | 65920 | 44196 | 146% ✅ ≥120% |
| c1-P1 | GET | 97561 | 67889 | 45468 | 144% ✅ ≥120% |
| c50-P1 | SET | 192938 | 189789 | 151035 | 102% ⚠ win<120% |
| c50-P1 | GET | 192271 | 190585 | 156519 | 101% ⚠ win<120% |
| c50-P16 | SET | 2577320 | 1883239 | 2183406 | 118% ⚠ win<120% |
| c50-P16 | GET | 2617801 | 2512563 | 2053388 | 104% ⚠ win<120% |
| c100-P1 | SET | 188005 | 188893 | 150489 | 100% ❌ LOSS |
| c100-P1 | GET | 187441 | 187864 | 154583 | 100% ❌ LOSS |
| c50-10KB | SET | 152555 | 149477 | 128949 | 102% ⚠ win<120% |
| c50-10KB | GET | 153846 | 152905 | 124533 | 101% ⚠ win<120% |

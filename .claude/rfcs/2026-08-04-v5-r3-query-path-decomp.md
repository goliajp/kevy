# R3:热窗内查询路径 —— Phase A decomposition 计划

> 总案 §R3。判据:热窗内一次分页查询的成本组成 18 段可陈述且
> ±20% 对账实测。本 RFC 只授权 **Phase A(read-only decomp)与其
> 前置 gate**;Phase B(attacks)在 decomp 产物 + gate 2 之后另立。
> 方法论:`.claude/rule/perf-vs-foss.md`(两步 dance + 双 gate)。

## 〇、靶(已有实测,PGCOMPARE-2026-07-26 round 1,in-cache)

单表 2M 行 ×440B,同 harness 同 box(lx64),PG18 stock:

| shape | PG18 p99 | kevy p99(everysec) | gap |
|---|---:|---:|---:|
| idx(二级索引 LIMIT 20)| 126 µs | 212 µs | ~1.7× |
| page(`WHERE dept=? AND age BETWEEN ? ORDER BY age LIMIT 20`)| 131 µs | 248 µs | ~1.9× |
| pk 点查 | 85 µs | 74 µs | kevy 赢 |

in-cache round 恰是"热窗内"的定义域;体检单结论"慢在查询路径
不在 I/O"即由 pk 打平 + idx/page 落后构成。gap < 1.5× 才允许谈
idiomatic 差距 —— 这两轴 1.7-1.9×,按方法论必属 abstraction 浪费,
decomp 有的挖。

## 一、Gate 1(pre-Phase-A):baseline variance

单轮数字不可上攻击列表(§1 "single run shows -X% loss" 反模式)。
行动:lx64(kevybench,干净分盒纪律,PG 容器按 pgcompare.sh
runbook root 起)round-1 全套 **×3**,idx/page 两轴取
median-of-3 + sample stdev。判定:
- 两侧 stdev ≪ gap(gap ~80-120µs)→ 轴成立,Phase A 开工;
- 任一轴 variance ≥ gap → 该轴除名,回总案重定靶。

## 二、Phase A decomp(gate 1 过后)

**形态**:read-only agent(Read/Bash/Grep,无 edit),产物
`bench/PERF-DECOMP-<date>-idx-page-vs-pg18.md`。

**kevy 侧生命周期段(预枚举,decomp 时细化到 18+)**:
RESP parse → cmd_resolve 路由 → extension fan-out(argv 克隆/
channel dispatch)→ shard:parse Query → with_ready_segment(锁+
catalog gen 检查)→ segment 树走(range 定位 + limit 走)→
hydration(FIELDS 每 hit 读行)→ chunk 编码 → origin reduce
(k-way merge + cursor)→ RESP 编码 → write。对照注意 4 shard
fan-out 的每段放大(SME 扇出税已知)。

**PG18 参照系**(源码下载到 /tmp/pg18-src,file:line 引用):
`nodeIndexscan.c` / `nbtsearch.c`(btree descent + 顺序走)/
`heapam.c`(元组取)/ `printtup.c`(wire 编码)/ executor 启动
成本(`ExecutorStart` 一次 vs kevy 每 shard parse)。

**硬门槛**(方法论 §2):18+ 段;两侧 µs 估算总和 ±20% 对账
gate 1 实测;高层计数断言(每查询 parse 次数、每 hit 行读次数、
chunk 字节数)必须 runtime counter 或 diag 例程实测验证 ——
source-only 必要不充分。

## 三、Gate 2(pre-Phase-B,本 RFC 不执行)

decomp Top-1 攻击面必须 perf-record 实测 ≥ 双位数 pp self-time
(lx64,idx/page workload),否则 µs 估算是 hand-wave
(§8 "memcpys are the gap" 学费)。Phase B 攻击轮过 gate 2 后另立。

## 四、不做的

- 本单元不动任何机制、不做 attack、不调 kevy 配置面。
- KNN/MATCH/GROUPS 形不在本轮(靶只有 idx/page 两轴)。

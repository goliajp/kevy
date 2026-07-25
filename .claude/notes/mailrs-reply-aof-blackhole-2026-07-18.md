# 回信:AOF 坏帧黑洞报告(2026-07-17)→ kevy 4.0 durability-trust arc 逐条账目

- **收件方**:mailrs(原报告 `mailrs/.claude/notes/kevy-feedback-aof-blackhole-2026-07-17.md`)
- **处理方式**:全部诉求(必须/期望/建议/文案)均入 4.0 范围,无一 defer;并以报告为引子做了整域设计(RFC `kevy/.claude/rfcs/2026-07-17-v4-durability-trust.md`),修的范围超出报告本身
- **验收面**:每一条都有可执行门(`bench/crashgate.sh` 进 CI 每 push 跑;另有 repligate / replaymemgate)
- **版本**:4.0.0(feature/v4 分支;crates.io publish 待定)

## 逐条归宿

| 报告条目 | 级别 | 归宿 | 落点 |
|---|---|---|---|
| §2.1 corrupt tail 隔离 + truncate(禁止黑洞化) | P0 | **落地** | 打开时把 `[corrupt_offset, EOF)` 复制到 `aof-<id>.aof.corrupt-quarantine.<unix_ts>`(fsync 过),**再**截断主文件到最后一条完整记录;隔离副本写不出来(磁盘满)则 open 直接失败、文件原样保留。crashgate 的 no-blackhole 探针把守:重启后新写永远活过下一次重启(commit `b80ebe54`) |
| §2.3 open 可观测 | P0 | **落地,三面** | `Store::open_report()`(`replayed_commands/replayed_bytes/elapsed_ms/dropped_bytes/corrupt/quarantine_paths/resynced_bytes`);`KevyMetric::Replay` 增 `dropped_bytes`/`corrupt`;服务器 `INFO persistence` 增 `aof_last_open_dropped_bytes`/`aof_last_open_corrupt`;C ABI 增 `kevy_open_report`(`aa8c95ee` + `14b15022` + 各门透传) |
| §2.4 显式 `shutdown()` | P1 | **落地** | `Store::shutdown()`:全 shard 真 fsync + feed 干净停机标记,之后写入拒绝为 `KevyError::Closed`(读不受影响),幂等、clone-safe——正是你们要的"信号处理两行"。C ABI 配 `kevy_shutdown`(`22653242` + `176c8919`)。另修一处审计洞:last-clone drop 原本 `maybe_sync`(everysec 窗口内可为 no-op)先于 feed 标记落盘,标记可能宣称 AOF 尾巴没有的耐久性——已改无条件 `sync_now` |
| §2.2 replay 帧边界 resync | P1 | **落地(比期望的更强)** | 没停在 RESP 启发式重扫:4.0 换了 AOF 记录格式 v2(`KEVYAOF2`,每记录 `[len u32][crc32c u32][payload]`),重同步点要求长度 + CRC + 恰好一条良构命令三者同时吻合(伪接受 ~2⁻³²/候选)。`replay_resync` 旋钮 opt-in(`[persistence]`/`Config::with_replay_resync`),默认仍 strict;跳过区间经 `resynced_ranges`/`resynced_bytes` 上报,`corrupt` 保持竖起(= 你们提的 best-effort + 汇报折衷)。你们的损伤形状(文件中部 8 字节)在 crashgate 实测 100500/100500 全恢复(`29354d26` + `f7d041ba`) |
| §2.5 writer 打开时尾帧自检 | P2 | **落地(与 replay 同源)** | `Aof::open` 与 replay 共用同一套前缀校验:打开即验尾、截坏帧、隔离,writer 从修复后的尾部追加——写侧与读侧同一个 verdict,不存在两边不一致 |
| §3.1 自动 rewrite 绝对阈值 | P1 | **落地(建议的两个都做了)** | `RewritePolicy` 三轨:growth 对(原有)+ **绝对上限** `auto_aof_rewrite_bytes`(= 你们建议的 `with_auto_rewrite_bytes(512<<20)` 原样)+ **陈旧度** `auto_aof_rewrite_interval_secs`;server/embedded 单决策点,CONFIG SET 可热调(`22653242` + `92c8f4ff`) |
| §3.2 replay 内存峰值 | P2 | **落地** | v2 replay 流式化,峰值 O(最大记录) 而非 O(文件):实测 37MB 日志 RSS 4MB;`bench/replaymemgate.sh` 把守上限(`2cc5e0b2`)。注:v1 老文件的 replay 仍走旧路径,首次 rewrite 升 v2 后进入流式——升级后先手动 `rewrite_aof()` 一次即可锁定收益 |
| §3.3 `rewrite_aof()` 点赞 + 文档 + RewriteStats 字段 | — | **落地** | `RewriteStats { pub keys, pub bytes }` 字段公开;persistence.md 新增 Operator runbook 节,rewrite 周期建议(三轨触发器怎么配)官方化 |
| §3.4 移除 `flush()` 别名 | P2 | **落地** | 4.0 API break 一并移除;幸存名 `flushall()` 自述其义(CHANGELOG "The API break" 节) |
| §1.2 WARN 文案 "killed mid-append" 列首因 | P3 | **落地** | WARN 文案已把 "process was killed mid-append" 列为最常见成因,stderr 重定向降为次要;并点名隔离文件路径 |
| §1.4-2 panic-quarantine 文档与行为对齐 | P3 | **落地(文档改真话 + 行为补齐)** | 虚构的 `panic-quarantine` 文件名从三语文档全部清除;真实行为(`corrupt-quarantine`、打开时先隔离后截断)写入 persistence.md 崩溃一致性契约节(EN/zh/ja 同步) |

## 报告之外,同一域里顺手关掉的

审计把整个 durability 域拆开重看,报告没点到但同类的洞:

1. **AOF 完整性**:v1 格式对 payload 位翻转零检测(静默重放污染值)。v2 的 CRC32C(硬件指令)当场拒绝——crashgate 的 payload-flip 格从红转绿。
2. **复制 offset aliasing**(与黑洞同构的"静默分歧"):主节点 unclean 重启后 offset 归零,副本带旧 offset 重连会被喂新历史的同号帧 = 永久静默分歧。4.0 握手带 feed generation(`REPLICATE FROM <gen> <offset> ID <id>`),gen 不匹配一律全量重同步;mid-stream FLUSHALL/promotion bump 的 stall/aliasing 同时关掉。
3. **副本本地 AOF 与键空间脱节**:快照重同步绕过提交路径,副本本地日志仍记旧历史;4.0 在重同步后同步重写本地 AOF。
4. **feed 干净停机标记先于 AOF fsync 的窗口**(见 §2.4 行内)。

## 你们宿主侧缓解措施的对应关系

- SIGTERM 优雅退出:保留——但现在有 `Store::shutdown()` 可显式调用,不必再"祈祷所有 Arc 释放"。
- admin 端点手动 `rewrite_aof()`:保留有益;三轨自动触发器配好后可作为兜底而非主力(建议 `auto_aof_rewrite_bytes = 512MiB` 起步,对应你们 2.2GB 事故的形状)。
- "启动日志必须看到 `(clean)`" SOP:升级为程序化——`open_report()` 的 `dropped_bytes/corrupt` 直接进健康检查,人不用再盯 stderr。persistence.md Operator runbook 节即你们 SOP 的官方化版本。
- maildir ground truth 重放:继续是最后防线;但 v2 + resync 后,"无外部 ground truth 的纯 kevy 用户"也不再面对黑洞/全弃两个结局。

## 升级注意(mailrs 视角)

- 3.18 数据目录零迁移打开;**首次 rewrite 后文件升 v2,不可再降回 3.18**(灰度窗口想留退路:窗口内 `auto_aof_rewrite_percentage = 0` + 先备份快照)。详见 docs/UPGRADING.md。
- `replay_resync` 默认关;按你们 "maildir 是 ground truth" 的姿态,建议常开。
- 现场样本:不再需要脱敏样本——crashgate 的 midfile-splice 格已按你们报告的损伤形状(帧头完好、bulk 载荷截断)做成确定性注入,每次 CI 都在复现你们的事故。

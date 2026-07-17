# RFC — v4 durability trust arc(AOF 可信化)

- **日期**:2026-07-17 · **状态**:USER-APPROVED SCOPE(全量进 v4,no defer,报告内外全覆盖)
- **引子**:mailrs P0 事故报告 `~/workspace/stables/mailrs/.claude/notes/kevy-feedback-aof-blackhole-2026-07-17.md`
  (AOF 坏帧黑洞:3.18.0 上 replay 停在撕裂帧丢尾、writer 从真实文件尾续写 → 每次重启回滚,3 天静默丢 231MB)
- **用户指令**:所有报告项(P0-P3、建议类、期望类)全部必选;设计面覆盖整个 durability 域,不限于报告点;排在 v4 ship 之前(AOF 格式演进押 4.0 未发布的唯一无痛窗口)

## 0. 现状账(v4 HEAD 逐条源码核实,2026-07-17)

| 报告项 | v4 现状 | 归宿 |
|---|---|---|
| §1 黑洞本体 | 已修(`31231d1d`:open 时 `valid_prefix_len_of_file`+truncate) | crashgate 固化为契约 |
| §2.5 writer 尾帧自检 | 已修(同上) | 同上 |
| §3.4 `flush()` footgun | v4 已移除 | 已闭 |
| §2.1 quarantine+truncate | 只 truncate,无 quarantine(`set_len` 直接销毁) | **T2** |
| §1.4-2 文档谎言 | `docs/persistence.md:194` 仍承诺 quarantine,行为没有 | **T2 + T7** |
| §2.3 open 可观测 | `KevyMetric::Replay` 无 dropped/corrupt 字段 | **T3** |
| §2.4 `shutdown()` | 无 | **T4** |
| §2.2 resync 恢复完好尾 | 无 | **T6**(靠 T5 格式 v2 做实) |
| §3.1 rewrite 绝对阈值 | 只有 pct+min_size | **T4** |
| §3.2 replay 内存峰值 | `read_to_end` 整读;黑洞修复引入 open 双读(valid_prefix 一遍 + replay 一遍) | **T5** |
| §1.2 WARN 首因文案 | 仍是 "non-kevy bytes" | **T3** |

已核实的健全面(crashgate 仍需注入验证):snapshot 写 = tmp+fsync+rename;AOF rewrite = `.rewrite` tmp+fsync+rename。

## 1. 域分解(报告之外的系统面 —— 全域 14 轴)

1. **AOF append 路径**(buffer/fsync 策略)— 现状 EverySec 缺省,健全
2. **AOF open/verify/replay/verdict/repair** — 本 arc 主战场
3. **AOF rewrite 崩溃原子性** — 已 tmp+rename,crashgate 验证
4. **Snapshot 崩溃原子性 + snapshot×AOF 组合恢复** — 已 tmp+rename,crashgate 验证组合序
5. **多 shard 一致性偏斜**:shards=N 各自 aof-N.aof,崩溃后各自独立截断 → shard 间恢复到不同时刻。契约必须成文(kevy 无跨 shard 原子性承诺 → 偏斜合法,但要写清 + crashgate 断言各 shard 自身前缀完整)
6. **Feed/CDC × 截断一致性 —— 已审计(2026-07-17,file:line 证据链在 T1 审计报告)**:feed ring 纯内存(`VecDeque`,open 时永远空建,只恢复 `(gen, next_offset)` 游标 sidecar);写路径 = AOF 入 buffer → feed push,**EverySec 下帧可先到消费者、后被截断抹掉 = phantom window ≤ fsync 窗口**(Always=0,No=无界);崩溃 → gen bump → 旧游标 FEEDRESYNC 强制 SCAN 重建 → 协议自洽,phantom 可检测但外部副作用泼出去收不回。**真洞 #1:embedded DropGuard 先 `maybe_sync`(1s 窗口内 no-op)后写 fsync 的 close 标记 → 掉电窗口内 gen 不 bump、游标续接、库回滚 = 唯一协议检测不到的 phantom。修复 = `sync_now()` 对齐 server shutdown_drain(已落,T1)**
7. **复制 × 截断 —— 已审计**:replica 走内存 backlog 流式 + TooOld/Future→全量 snapshot ship,不走 AOF;慢重连路径安全(Future 臂按 forked-history 丢弃)。**真洞 #2(broken):握手 `REPLICATE FROM <offset>` 不带 generation → primary unclean 重启(gen bump、offset 归 0)后新写数追过旧游标时,`frames_from` 用新历史的同号 offset 喂 replica = 永久静默分歧;心跳虽带 gen 但无代码比较触发 resync。修复 = T8(gen 入握手,wire 改动押 4.0 窗口)。**附带核查项:server-as-replica 的自身 AOF 在 snapshot resync 时不重置(flushall 直调不走 commit 路径),依赖重启后全量重同步兜底 — T8 一并审
8. **可观测性**:OpenReport / metrics 字段 / server INFO persistence 节
9. **生命周期 API**:shutdown() 语义、与 drop/close 的关系、各语言门透传
10. **配置面**:fsync 策略与 rewrite 阈值经 C ABI/各门可达(现状:FFI dir-open 锁死默认值,mobilegate durable 轴已撞到)
11. **格式演进**:AOF envelope v2(本 RFC 承重决策,见 §2)
12. **测试面**:crashgate(SIGKILL 注入矩阵)+ replay/resync fuzz + cov ratchet
13. **文档**:persistence.md 状态机重写 + operator runbook(mailrs SOP 官方化)+ WARN 文案
14. **大值写放大**(RN durable 轴 4KB 0.1× vs MMKV):**显式决策 = 不做存储模型改造**。依据:mmap-AOF 后端已完整实现并被实测否决(`bench/PERF-FINDING-2026-07-15-mmap-aof-refuted.md`,5-6× 更慢);缓解 = T4 rewrite 阈值(控文件体量)+ 文档如实边界。这是 measured refutation 下的关闭,不是 defer。

## 2. 承重决策:AOF envelope v2

**一个格式动作同时解决四件事**:resync 可靠性(§2.2)、流式 replay(§3.2)、篡改/撕裂检测(CRC)、廉价 valid_prefix(消 open 双读)。

```
文件头:  KEVY-AOF2 magic + u16 format-version
每记录:  [u32-LE payload_len][u32-LE crc32c(payload)][payload = 原 RESP multibulk]
sync 点:  每 ≥4MiB 插入 marker 记录(payload = "#SYNC\r\n<abs_offset>",自身也带 len+CRC)
```

- **恢复 = 结构化扫描**:坏记录后向前逐字节找下一个 `len+CRC` 自洽点(sync marker 让最坏扫描距离有界)。resync 从启发式 RESP 猜测变成 O(gap) 的确定性算法,误接受概率 ~2^-32 每候选点。
- **流式 replay**:按记录边界读,峰值 = O(单条最大记录),不再 read_to_end;valid_prefix 与 replay 合一遍扫描(黑洞修复的双读一并消掉)。
- **兼容契约**(修订 CHANGELOG 4.0.0 的 "disk format carries over unchanged" 表述):**v1(3.x)只读兼容,v2 只写**。open 认两种 magic;v1 文件首次 rewrite/显式 `rewrite_aof()` 时升 v2;`docs/UPGRADING.md` 写明"4.0 打开 3.x 数据目录零操作,首次 rewrite 后格式升级,不可回退到 3.x 打开"。
- snapshot 格式不动(已有自己的 header+原子写)。

## 3. Train 分解(线性,五轴收口各自适用)

### T1 —【gate 先行】crashgate + 契约文档
- `docs/persistence.md` 重写为状态机契约:open→verify→replay→verdict→repair→append 每态定义 + **丢失上界表**(坏帧事故损失 ≤ 坏帧本身 + fsync 窗口;永不黑洞);多 shard 偏斜契约;feed×截断契约(先审计后成文)
- `bench/crashgate.sh`:SIGKILL 注入矩阵 = {mid-append, mid-fsync, mid-rewrite, mid-snapshot, mid-feed-emit} × {everysec, always} × {单 shard, 4 shard};断言 = 丢失上界 + 无黑洞(重启两次,第二次 replay 停点不回退)+ quarantine 文件存在且内容正确 + rewrite/snapshot 原子性
- **先让 gate 红**:红项清单 = T2-T6 的验收面。复用 mmap gate suite 的 torn-marker 注入基建(`68b06565`)
- Feed×截断、replica×截断的审计结论回填本 RFC §1.6/§1.7

### T2 — corrupt tail 收尾:quarantine + truncate(P0)
- replay/open 发现坏帧:`[corrupt_offset, EOF)` 先拷贝到 `aof-<id>.aof.corrupt-quarantine.<unix_ts>`,再 truncate 主文件
- 与 panic-quarantine 行为、命名、docs 三方对齐(§1.4-2 的文档谎言就此闭合)
- crashgate 断言转绿

### T3 — 可观测性(P0)
- `Store::open_report() -> OpenReport { replayed_commands, replayed_bytes, elapsed_ms, dropped_bytes, corrupt, quarantine_path }`
- `KevyMetric::Replay` 增 `dropped_bytes` / `corrupt`(`#[non_exhaustive]` 已留门,additive)
- server `INFO` persistence 节增 `aof_last_open_dropped_bytes` / `aof_last_open_corrupt`
- WARN 文案首因改 "process killed mid-append"(§1.2)
- C ABI:`kevy_open_report(db, out)`(additive 符号);各语言门透传(Swift/Kotlin/TS/Dart/N-API/Go/C#)

### T4 — 生命周期与策略面(P1)
- `Store::shutdown(&self) -> io::Result<()>`:flush 全 shard AOF + feed close marker + fsync + 拒绝后续写(幂等,clone 安全,信号处理两行收尾)
- rewrite 触发策略:`with_auto_rewrite_bytes(u64)` 绝对阈值 + `with_auto_rewrite_interval(Duration)` 时间阈值,与现有 pct/min_size 组合(任一命中即触发)
- `RewriteStats` 字段公开(keys/bytes)
- C ABI:`kevy_shutdown`(additive);config 面过 FFI:`kevy_open_with(dir, opts)` 或按位 opts 结构(fsync 策略 + rewrite 阈值),各门透传 —— mobilegate durable 轴顺带获得公平旋钮

### T5 —【RFC 已定】AOF envelope v2 + 流式 replay(P1)
- §2 全量:双 magic 读、v2 写、rewrite 升格式、sync marker
- 流式 replay(峰值 O(最大记录));valid_prefix 与 replay 合并单遍
- memgate 断言:2GB 级 AOF replay 峰值 < 128MiB(lx64 实测入账)
- diskgate:v2 头部开销(8B/记录)对 mailrs 形状(13.7M 条/2.2GB ≈ +5%)入账;rewrite 后体积对比

### T6 — resync:坏帧后恢复完好尾(P1)
- v2 格式上的确定性重扫(§2);默认 **strict**(停在坏帧,quarantine 尾部 = T2 行为),`with_replay_resync(true)` 启用 best-effort:跳过坏记录续放,`OpenReport` 增 `resynced_ranges: Vec<(u64,u64)>`
- v1 文件的 resync:RESP 帧头启发式扫描(尽力而为,文档如实标注可靠性差异)
- fuzz:随机损伤注入 × resync 恢复率断言;231MB 场景复现件(mailrs 提供的脱敏样本形状)进 crashgate 回归

### T8 — 复制世代栅栏(审计洞 #2;wire 改动押 4.0 窗口)【已实施】
- 握手升 `REPLICATE FROM <gen> <offset> ID <id>`(6 args),ACK 升 `+ACK <gen> <offset>`;5-arg / 单数字 ACK 为 4.0 clean break(WrongArity / AckMalformed)
- **fence 落点在 pump 而非握手**(实施精化):`ReplicaState` AckSent/Streaming/SnapshotShipping 全带 `generation`;`fill_streaming_output` 每轮先查 gen 再查 caught-up —— 修掉审计外第二张脸:mid-stream FLUSHALL/promotion bump 后 `sent_offset >= next` 读作"caught up" = 永久 stall,直到新历史追过旧游标开始吐 aliased 帧。规则:`gen==feed_gen → 照常;gen==0 && sent==0 → 采纳(fresh 语义,测试兼容);其余 → snapshot ship`
- replica runner(kevy + kevy-embedded)维护 `data_gen`:SnapshotEnd / 从 offset 0 连上时采纳 ACK gen;心跳 gen ≠ 握手 gen → 断链重连(双保险)
- embed-as-writer(replica_source.rs)原本完全无 gen(裸内存 backlog,每次重启 offset 归 0 = 同一个洞)→ per-boot gen = spawn 时刻 nanos,握手 fence 同规则
- 附带核查落地:`apply_snapshot_end` 后同步 `Aof::rewrite_from(&store)` re-base 本地 AOF(先 drain 在途 persist job,防 stale tmp 事后 rename 覆盖 rebase);此前 flushall+load 绕过 commit 路径,resync 后本地日志与键空间脱节,primary 不在线时重启会 serve 错误合成态
- 测试:embed_writer_e2e `writer_restart_generation_fence_ships_instead_of_aliasing`(重启 writer 追过旧游标 → 必须 ship 全量);kevy replication `unclean_restart_generation_fence_ships_instead_of_aliasing`(同 dir unclean 重启 gen 1→2,旧 `(gen,offset)` claim → SnapshotBegin 而非帧续传);repligate clamp 5(writer SIGKILL 重启 → runner data_gen 走 fence → 收敛 + digest 稳定)

### T7 — 文档与 runbook 收口
- persistence.md 三节:状态机契约(T1)/ operator runbook(优雅关闭 SOP、启动日志检查项、rewrite 周期建议 —— mailrs §4 官方化)/ 格式 v2 说明
- UPGRADING.md 格式升级契约;CHANGELOG 4.0.0 修订 disk-format 行 + 本 arc 全量条目
- mailrs 回信账目(逐条诉求 → 归宿)

## 4. 明确关闭项(有据,非 defer)

- **3.x backport(3.18.1)**:用户指令"都放在 v4 范围中解决"→ 不重开 3.x 发布线;mailrs 宿主侧已缓解(优雅关闭 + 手动 rewrite),根治 = 升 v4
- **大值写放大的存储模型改造**:mmap-AOF 已实测否决(§1.14);value-log/LSM 分离超出 kevy charter(纯内存 store + append log),不立项

## 5. 验收(arc 级)

五轴 + crashgate:perfgate 无回归、memgate 新增 replay 峰值线、diskgate 新增 v2 开销线、cov ratchet 不降、docs 三处收口、**crashgate 全绿 = arc finish 的硬门**。ship 顺序:本 arc 全绿 → 既有 ship 清单(brew/apt/npm/tag v4.0.0)。

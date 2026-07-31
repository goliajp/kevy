# R2b:冷段形态 —— 一块段石头,三种冷段,一个新的 durability 决策

> R2 RFC 后半之一(前半 = `2026-07-31-v5-r2-window-model.md`;R2c 滑动机制另成篇)。
> 输入:R2 前半 §四(温度相变)/§六/§八(R4a 数据:text 是主战场、flag 全热、
> 窗口列 ORDERPATH 树切点直摘);增补拍板(SQLite 参照合法、KV/pubsub 钉死、架构 clean)。
> **设计文档;每个 train 落地前照例 gate 先行。**

## 一、取证补充(本轮新增,file:line)

1. **text 的 docs 表存全文**:`kevy-text/src/segment.rs:240`
   `docs.insert(key, (id, dl, fields.to_vec()))` —— 每 doc 的原文字段留在内存,唯一用途是
   update/removal 时 re-derive 该 doc 自己的 token(`weighted_tf(&old_fields)`,O(doc) 不
   O(index))。**出窗 doc 不再被 update ⇒ 冷 doc 的原文是纯冷负担**;doc_bytes 公式
   `2·key + text + 110` 说明它是 423MB floor 的结构性大项之一(与 postings 并列)。
2. **行序列化前例**:`kevy-persist/src/snapshot_payload.rs:24` `write_hash_payload`
   (field-TTL 共序列化的坑已在 capacity RFC 踩平)。
3. **倒排冷编码前例**:`kevy-text/src/docblobs.rs` 的 delta+varint(Lucene 同款)已在
   positions 通道生产;冷 posting 的编码不需要新发明。
4. **scalar 冷段的天然形态**:`Segment.tree = BTreeSet<(IndexValue, key)>` 的出窗前缀
   摘除后,冷段就是**按 (value,key) 排序的不可变数组** —— 二分即查,零结构开销/条目。

## 二、段石头:`kevy-seg`(新石头,SQLite 参照的落点)

三种冷段(行段 / scalar 索引冷段 / text 冷段)共用一块**不可变有序记录段**石头:

- **模型**:`SegBuilder`(一次构建:追加有序记录 → 页化 → CRC32C per page → footer)
  与 `Seg`(打开:mmap 或 pread;`get / range / count_range` 按段内有序键二分)。
  记录 = (key_bytes, payload_bytes),键序由构建方保证,段自身只校验单调。
- **页内布局学 SQLite,削到只读**:槽目录页(cell pointer array,页尾向前长)+ 溢出页
  (超页记录链出);**不学**:WAL/freelist/游标栈/varint 页头(不可变段不需要)。
  页 4K 对齐,mmap 时按页 madvise;pread 时按页读。
- **footer**:min/max key、记录数、页数、构建时 wall/mono 戳、可选 per-page fence keys
  (段内二分先走 fence 目录,一次页读定位)。
- **charter**:纯 no_std 核 + std 的 mmap/文件层(kevy-sys 前例);fuzz(构建-重开往返、
  截断/翻位拒绝)、bench(build 吞吐 / 点查 / range count)独立齐备;semver 石头判据全过。

**为什么一块石头而不是三个**:三种冷段的差异全在**记录的语义**(行 payload / 索引条目 /
posting blob),而定位、页化、校验、生命周期(build-once/immutable/unlink)完全同构 ——
差异放 payload 编码器,同构放石头。架构 clean 铁律的直接应用。

## 三、三种冷段

### 3a. 行段(温度相变的冷面)

- 记录 = (window_value ‖ pk_key) → `write_hash_payload` 序列化的行(field-TTL 共序列化)。
  键前缀是窗口列值 ⇒ **段内时间聚簇**(R2 前半的"免费聚簇"落到字节上):
  按窗口区间的 range 读 = 顺序页扫。
- 热层残留:出窗行在 keyspace 中留 **ColdRef 级 stub 还是零残留?** 设计:**零残留 +
  段目录兜底**。SCAN/EXISTS 语义要求行可见 —— 由**段目录**(per-shard,每段
  min/max window_value + 记录数 + bloom(pk))回答存在性;GET/HGETALL 冷键 =
  段目录 bloom 命中 → 段内二分。凭据:窗口表的行键含窗口语义(R4a:天然键统治),
  stub 每行 96B × 千万行 = 数百 MB,恰是要消灭的对象;bloom + 目录是 per-段常数。
  **这是对现 tiering stub 模型的偏离,风险最高的决策**——KV 面(非窗口表)不受影响
  (铁律一),但窗口表的 SCAN 全量语义要走"热层扫 + 段目录扫"双源归并。负结果预留:
  若双源 SCAN 的语义/性能不可守,回退 stub 模型(容量账退到 stub 上限)。
- 复活(R4a 修订):写 pk 在冷段 → 段目录命中 → 读回行 → 热层插入 → 段侧墓碑
  (段不可变 ⇒ 墓碑在段目录的 delete-bitmap,compact 时物理消)。

### 3b. scalar 索引冷段

- 记录 = (value ‖ key) → 空 payload(条目即键)。出窗 = 树前缀摘除 → SegBuilder 直灌
  (已序,零排序成本)。查询 = 热树 range + N 段 range 的 k-way 归并(段带 fence 目录,
  每段 O(log) 定位 + 顺序读)。
- **`IDX.COUNT` 跨窗 = 热 count + Σ 段 `count_range`(二分差,O(log)/段)** ——
  R4a 的两个"引擎缺"红项之一(claused COUNT)在冷段侧天然有解;热段的 claused count
  作为同 train 的引擎补齐(FILTER 谓词下推到 count 游走,不物化行)。
- 非窗口列索引(R2 §六):flag 小索引全热(R4a 实测背书);**大的非窗口索引**
  (unique email 类)首版全热 + 记录"按行归属段化"为 v-next(行出窗时其索引条目
  跟随迁段,需要 back-map 反查,复杂度留档)。

### 3c. text 冷段(主战场,423MB 的正面)

按**窗口桶**(建议 = 滑动粒度,R2c 定,预设一天)构建**桶级迷你倒排段**:

- 出窗桶的 doc 集合 → 构建:token → delta-varint posting(docblobs 编码前例)、
  doc 表(**不含原文** —— 冷 doc 不再 update,re-derive 用途消失;只留 dl 与 key 映射)、
  df/total_len 桶级统计(跨段 BM25 合并走"global BM25 跨 shard 求和"的既有形状,
  段=另一种 shard)。
- 查询:热段照旧;跨窗 MATCH = 热 + 命中窗口区间的桶段归并(每桶一次 token 定位)。
  桶数有界(span/粒度);`IN <field>`/positions 通道同构冷编码。
- **收益结构**(可陈述,数字为推论):冷 doc 的原文字节(docs 表大项)+ postings 结构
  开销(Buckets/HashMap → 扁平 varint)整体出内存,只留桶段 footer/fence 的 per-段常数;
  text floor 从 ∝ 全量 docs 降到 ∝ 热窗 docs。

## 四、durability 决策:段是持久真相(与 vlog 的根本分岔)

vlog 是 per-boot 可弃(AOF 是唯一真相)。冷段**不能**沿用:出窗行若只活在段里而段可弃,
boot 就要从 AOF 重建全部历史段 —— 与"数据 ≫ 内存"的 boot 时间目标直接冲突。定稿:

- **段文件持久**:build 完成 = 数据页 fsync → footer 写入 → fsync → **manifest 原子追加**
  (manifest = 段目录的持久形态,append-only + 定期紧缩,crash 语义 = 未入 manifest 的
  段文件是垃圾,启动清扫)。
- **AOF/rewrite 联动**:行出窗后,rewrite 的输出不再含该行的命令流(它在段里);
  rewrite manifest 记录"本 AOF 的键空间 = 热层 + 段集 {S1..Sn}"。**恢复真相 =
  AOF + snapshot + 段集**,三者由 manifest 缝合。
- **备份语义升级**(R2 前半思路 4 兑现):段不可变 ⇒ 增量备份 = 拷新段 + manifest;
  `备份=拷文件` 的 S5 句子保持为真,且变强(增量天然)。
- **crashgate 义务**:新增段面 crash 矩阵(build 中途 kill / manifest 追加中途 kill /
  段引用与 AOF rewrite 竞态)——这是本 RFC 最重的工程义务,gate 先行。

## 五、判据(结构性,gate 承载)

- **S1 终局式推进**:窗口表内存 = 热窗(∝ 工作集)+ Σ段(footer+fence+bloom 常数,
  公式化)+ 目录;`能装多少业务由工作集决定`在 text 表上首次可陈述。
- 热窗内查询与无窗口时 **byte-identical**(A5 体裁,门禁);KV/pubsub 赢线保持(铁律一)。
- 跨窗查询冷读可计数:每查询 `cold_segs_touched` / `cold_pages_read` 计数器,
  `段数 × O(log)` 的公式与实测对账。
- 段面 crash 矩阵全绿;备份=拷文件 e2e(含段)。
- 复活路径 e2e(snooze 形状:出窗 → 写窗内值 → 回热窗 → 段墓碑 → compact 消)。

## 六、train 拆分(建议)

1. **T-seg**:`kevy-seg` 石头(builder/reader/fence/CRC + fuzz + bench)——无引擎接线,纯石头。
2. **T-scalar**:scalar 冷段接线(树前缀摘除 → 段;查询归并;跨窗 COUNT)+ 热段 claused COUNT。
3. **T-manifest**:段 manifest + AOF/rewrite 联动 + crash 矩阵(重,单独成列)。
4. **T-row**:行段 + 段目录/bloom + 双源 SCAN + 复活。
5. **T-text**:text 桶段(主战场,依赖 1/3)。
6. R2c(滑动机制)与 T2-T5 交织,另篇 RFC。

## 七、发散区(本轮新增思路)

1. **fence keys 直接做温段**:段 fence 目录常驻内存本身就是"温层"的最小形态——
   Elias-Fano 温段(R2 前半思路 2)可以先不做,fence 粒度调细就是 80% 效果。
2. **段即复制单元**:replica full-sync 传段文件而非命令流(不可变 ⇒ 可校验可断点续传),
   复制追赶时间与段大小线性——记入 R6/复制面的远期。
3. **冷段延迟解码**:行段 payload 保持 AOF 帧编码(SET/HSET 命令流原样)——恢复 = 直接
   replay 段?否:那是 vlog 思路的残影,放弃(查询要二分,命令流不可索引)。记为已拒。

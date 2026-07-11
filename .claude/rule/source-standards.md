# 源码规约(v4 T1c 成文,hard rule)

对象 = `crates/*/src` 的全部源码。三章:注释、pub API、LOC。
每条都带 why——规则的边界判断以 why 为准,不搞字面官僚主义。
机械可检的部分由三个 gate 兜底:**commentgate**(本文 §1)、
**locgate**(§3)、clippy `-D warnings`(全局)。

---

## 1. 注释规约(commentgate 兜底)

### 1.1 唯一原则:注释只写"这段代码的语义与约束"

注释回答的问题是"**为什么是这个形**"——不变量、边界条件、取舍
论证、非显然的契约。注释**不**回答"它经历了什么"——哪个版本加的、
哪个批次改的、谁反馈的、上一版长什么样。

**Why**:代码的读者(crates.io 用户、贡献者、半年后的自己)手里
只有这份源码。历史叙事对他是噪音,内部代号对他是死链;而语义与
约束是他唯一需要、也只能从注释获得的东西。历史沿革有专门的家:
**git log(逐行,`git blame` 可达)与 CHANGELOG(逐版本)**——
在注释里复述历史 = 存两份必然漂移的账。

```rust
// BAD  — 叙事,读者无法解引用:
// K4 (v1.25 A.9, 2026-06-22): re-queue if more work remains.

// GOOD — 语义与约束,自足:
// A chunked writev may leave bytes in `output`; re-queue the conn so
// the next arm visit re-preps the send SQE — dropping it here would
// stall the reply until an unrelated readiness event.
```

### 1.2 禁词清单(commentgate 机械检测,词表 = `bench/commentgate-terms.txt`)

| 禁 | 例 | Why |
|---|---|---|
| 版本标记 | `v1.25` / `v3.16.2` | "哪版引入"属 git log/CHANGELOG;需要版本号才讲得通的注释是在讲历史,不是讲眼前的代码 |
| 内部代号 | `mailrs` / `luna`(裸用)/ `lx64` | 姊妹项目名、盒名对外部读者是死链。例外:`luna-core` 是真实 crates.io 依赖名,API 层注释可引用 |
| 批次代号 | `T1.5.6` / `W5` / `K-103` / `K4` | 私有 plan 坐标,repo 外不可解析 |
| RFC/plan 引用 | `RFC 2026-07-04` / `RFC D4` / `RFC LOCKED` / `plans/20…` | 内部设计文档不随 crate 发布;引用 = 悬空指针。IETF 真 RFC(`RFC 3174`)合法,词表只匹配内部引用形 |
| 裸日期 | `2026-06-22` | 注释里的日期是日记,不是代码说明 |
| 开发叙事 | "XX 反馈后加的" / "上一版是…" / 中文叙事标志 | 同 1.1;且注释按项目惯例是英文,中文叙事是 session 文本外溢 |

**改写方向**(T2 sweep 的工作定义):每处命中二选一——
(a) 提炼出注释想说的**语义/约束**,用通用工程语言重写;
(b) 若剥掉叙事后什么都不剩,**整行删除**(git log 留档,无损失)。

### 1.3 SAFETY 注释义务(既有惯例成文)

每个 `unsafe` 块/`unsafe fn` 调用点必须有 `// SAFETY:` 行,写清
**为什么此处的前置条件成立**(不是复述"这是 unsafe")。

**Why**:`unsafe` 是编译器验证的空洞,SAFETY 注释是把验证责任
显式转交给人的收据。没有收据的 unsafe 在 review 与后续改动时
无从审计——改动者不知道哪些前置条件是自己要维持的。

```rust
// SAFETY: `idx < self.slots.len()` was checked by the caller's
// bounds test above; slots are never shrunk while a shard runs.
unsafe { self.slots.get_unchecked(idx) }
```

---

## 2. pub API 规约(v4 一次定型,semver 大版本是唯一 break 窗口)

### 2.1 构造命名二分法:资源 = `open`,网络 = `connect`

打开/创建本地资源(store、embedded、文件形态)一律 `open`;
建立网络连接(client、resp-client)一律 `connect`。不出现第三个
动词(`new` 保留给纯内存、无失败面的值构造)。

**Why**:动词即契约——`open` 暗示持久资源与独占/复用语义,
`connect` 暗示对端可能不在、可重试。二分法让用户不读文档就能
猜对入口,32 个 crate 一致后,学一个 = 学全部。

### 2.2 错误单一币种:`KevyError`

对外错误面收敛到顶层 `KevyError`(含 StoreError 语义)。禁止:
把结构化错误降级成 `io::Error::other`;用 `&'static str` 当错误;
每个 crate 各造一套对外 error enum。crate 内部错误类型允许存在,
但跨出 pub 边界前必须换币。

**Why**:错误类型是 API 的一半。用户的 match 臂写在哪个类型上,
哪个类型就是契约;降级成 io::Error 等于把结构化信息焊死在
Display 字符串里,用户只能 parse 文本——那是我们自己最恨的形。

### 2.3 Builder 惯例

多于 2 个可选配置项的构造走 builder(`Type::builder() … .build()`
或 `open_with(Config)`);builder 方法 = 字段名、consuming self、
`build()`/终动词收尾。不搞 `new_with_x_and_y` 组合爆炸,不搞
半 builder 半 setter 的杂交形。

**Why**:可选项的笛卡尔积只有 builder 扛得住;统一惯例后新增
配置项是 minor 而非 breaking。

### 2.4 `#[must_use]` 于查询型返回

纯查询(不产生副作用、返回值即全部意义)的 pub fn 与"必须被
消费才有意义"的返回类型(guard、handle、Result 之外的验证结果)
标 `#[must_use]`。

**Why**:查询结果被丢弃 100% 是调用方 bug;让编译器在写下 bug
的那一刻报警,比 code review 便宜两个数量级。

### 2.5 面积纪律(既有规则重申)

新 crate 先想 API surface:常用入口 pub fn ≤ 7-10 个;复杂内部
`pub(crate)` 隔离。**Why**:pub 出去的每个符号都是永久负债
(semver 约束下只能加不能改);面积小的 API 才可能"一次定型"。

---

## 3. LOC 规则(locgate 兜底)

文件 ≤ 500 LOC、函数 ≤ 50 LOC、waiver 仅限数据驱动 dispatch/match
表(`// LOC-WAIVER:` 行)——细则与检测口径以
[`bench/locgate.sh`](../../bench/locgate.sh) 为准(单一事实源,
本文不复述阈值以免两处漂移)。test 文件豁免(社区惯例)。

**Why**:500/50 不是美学,是 review 半径——石头层 bug 放大到
全调用方,能被一屏读完的单元才可能被真正 review 过。

---

## Gate 一览

| Gate | 检测面 | 词表/口径 | CI |
|---|---|---|---|
| commentgate | 注释禁词(§1.2) | `bench/commentgate-terms.txt`(gate 与 T2 sweep 共用) | `bench/commentgate.sh`(T2 清零后启用,ci.yml 已备注) |
| locgate | 文件/函数 LOC(§3) | 脚本内置 | `bench/locgate.sh`(已启用) |
| clippy | 全量 lint | `-D warnings` | 已启用 |

对外版规约(贡献者视角的精简形)= 根目录 `CONTRIBUTING.md`;
本文是内部完整版,两者冲突时以本文为准并同步修正。

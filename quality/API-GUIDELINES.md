# Rust API Guidelines —— kevy 逐条核对

外尺来源:`https://rust-lang.github.io/api-guidelines/checklist.html`(49 条,
2026-09-08 取回原文)。SemVer 判定依据:
`https://doc.rust-lang.org/cargo/reference/semver.html`(同日取回)。

**属主拍板(2026-09-08):走 v6.4。**「代码质量改变一般情况也不会动 API。」

因此本表把每条分成三态:

- **`6.4`** —— 修复是 MINOR,本版做
- **`v7`** —— 修复必然破坏 API(带 Cargo 官方标识符),**显式记为 v7 待办,
  不是悄悄放弃**
- **`n/a`** —— 不适用,写明理由

覆盖面:41 个可发布 crate(`publish = false` 的不计)。

**格子里不允许填「大致符合」。** 每格是 `pass` / `fail:<数量或 file:line>` /
`n/a:<理由>`。

---

## 状态汇总(机械可判的 12 条已跑完)

| 条目 | 现状 | 归属 | 说明 |
|---|---|---|---|
| **C-PERMISSIVE** | `Apache-2.0 OR MIT` | ✅ pass | |
| **C-STABLE** | 零第三方依赖 | ✅ pass | 公开依赖只有自家 crate,同版本线 |
| **C-CASE** | rustc 命名 lint 已 `deny` | ✅ pass | `warnings = "deny"` 覆盖 |
| **C-METADATA** | **13 / 41 crate 缺项** | **6.4** | 多缺 `documentation` / `homepage`;`kevy-tmpdir`/`time`/`scalar`/`seg`/`window` 另缺 `keywords`/`categories`/`readme` |
| **C-DEBUG** | 392 个公开类型,**173 个无 `Debug`** | **6.4** | 加 derive 是 MINOR。store 31 / cli 14 / uring 12 / text 11 / rt 9 / embedded 9 / map 8 / sys 7 |
| **C-QUESTION-MARK** | doc 示例里 **144 行**用 `.unwrap()` | **6.4** | 官方要求用 `?` |
| **C-EXAMPLE** | 1,887 `pub fn` / **195 示例** | **6.4** | 石头层优先;示例即 doctest |
| **C-FAILURE** | `# Errors`/`# Panics`/`# Safety` 段 **48** 处 | **6.4(部分)** | ⚠️ 见下「已有的有据偏离」 |
| **C-HIDDEN** | 未核 | 待核 | |
| **C-NO-OUT** | 未核 | 待核 | |
| **C-COMMON-TRAITS** | 未逐类型核 | **6.4** | 加 trait 实现是 MINOR(`item-new`) |
| **C-SEND-SYNC** | 未核(需编译期检查) | **6.4** | |
| **C-STRUCT-PRIVATE** | `pub struct` 内 **740 处 `pub` 字段** | **🔴 v7** | 见下 |

## 🔴 必须推到 v7 的(带官方 SemVer 标识符)

修复这些必然破坏 API,**不能装进 minor**:

| 条目 | 为什么破坏 | Cargo 标识符 |
|---|---|---|
| **C-STRUCT-PRIVATE** | 给全 `pub` 字段的 struct 加私有字段 | `struct-add-private-field-when-public` |
| **C-CASE / C-CONV / C-GETTER / C-ITER / C-WORD-ORDER** 的**既有违反** | 重命名任何公开项 | `item-remove` |
| **C-CUSTOM-TYPE**(`bool` 参数改枚举) | 改函数签名 | `fn-generalize-mismatch` |
| **C-SEALED** | 给已发布 trait 加密封超 trait | `trait-new-item-no-default` |
| 事后补 `#[non_exhaustive]` | 本身即破坏 | `attr-adding-non-exhaustive` |

**这些条目在 v6.4 里的动作是:核出来、写下来、不修。** 修在 v7。
核出来本身有价值 —— 它是 v7 的输入,而且防止新代码继续制造同类问题
(新写的 struct 从一开始就私有字段,是 MINOR,可以在 6.4 做)。

## ⚠️ 已有的有据偏离(不是 fail)

**C-FAILURE / `missing_errors_doc`** —— workspace `Cargo.toml` 已显式
`allow`,理由写在原地:

> 错误语义写在每个函数 doc 正文的散文里,不做成强制 `# Errors` 标题。

这是**刻意偏离**,不是遗漏。本表记为 `deviation:有据`,并在 v6.4 复核该理由
是否仍成立。`# Panics` 与 `# Safety` 两段**不在**这条 allow 的范围内,
仍按 pass/fail 核。

---

## 待核的 37 条(需判断,非机械)

Naming:C-CONV / C-GETTER / C-ITER / C-ITER-TY / C-FEATURE / C-WORD-ORDER
Interop:C-COMMON-TRAITS / C-CONV-TRAITS / C-COLLECT / C-SEND-SYNC / C-GOOD-ERR / C-NUM-FMT / C-RW-VALUE
Macros:C-EVOCATIVE / C-MACRO-ATTR / C-ANYWHERE / C-MACRO-VIS / C-MACRO-TY
Docs:C-CRATE-DOC / C-LINK / C-HIDDEN / C-RELNOTES
Predictability:C-SMART-PTR / C-CONV-SPECIFIC / C-METHOD / C-NO-OUT / C-OVERLOAD / C-DEREF / C-CTOR
Flexibility:C-INTERMEDIATE / C-CALLER-CONTROL / C-GENERIC / C-OBJECT
Type safety:C-NEWTYPE / C-CUSTOM-TYPE / C-BITFLAG / C-BUILDER
Dependability:C-VALIDATE / C-DTOR-FAIL / C-DTOR-BLOCK
Debuggability:C-DEBUG-NONEMPTY
Future proofing:C-SEALED / C-NEWTYPE-HIDE / C-STRUCT-BOUNDS

## 已核实的 n/a 与偏离(2026-09-08)

| 条目 | 判定 | 证据 |
|---|---|---|
| **Macros 全 5 条**(C-EVOCATIVE / C-MACRO-ATTR / C-ANYWHERE / C-MACRO-VIS / C-MACRO-TY) | **n/a** | `proc-macro = true` 0 个;`#[macro_export]` 0 处;`macro_rules!` 仅 2 处且都在 `tests.rs`,不导出 |
| **C-SERDE** | **n/a** | 零第三方依赖,不引 serde |
| **C-BITFLAG** | **deviation:有据 + 一处真问题** | `bitflags` 是第三方 crate,与 0-dep 宪章冲突,不能引 —— 这部分是有据偏离。**但该条的实质意图(一组标志不要用一堆 bool)有一处真违反**,见下 |

### 🔎 C-BITFLAG 实质违反:`NotificationFlags`

`crates/kevy-config/src/schema.rs:240` —— **16 个 `pub bool` 字段**的
`pub struct`,而且经 `crates/kevy-rt/src/lib.rs:198` 再导出,**在两个 crate
的公开面上**。

这一条同时命中三处:

- **C-BITFLAG** — 一组标志应当是位集合,不是 16 个 bool
- **C-STRUCT-PRIVATE** — 全 `pub` 字段
- **[[module-craft]] §11.5 L2** — 「位打包」消除的是内存与 cache 行占用;
  这里 16 个 bool = 16 字节,一个 `u16` 够

修法(newtype over `u16` + 具名常量,自实现不引 crate)**会改公开面 →
`item-remove` / `type-layout` → v7**。v6.4 只记录。

### ⚠️ 0-dep 宪章的一处待判(转 C 层架构问题)

机械核实全部第三方依赖:

| crate | 依赖 | 性质 |
|---|---|---|
| `kevy-client-async` | `tokio` / `smol` / `async-std` | **optional**,feature 门控的运行时适配 + dev-dep。引擎核心仍 0-dep,可辩护 |
| **`kevy-lua`** | **`luna-core`** | **非 optional 的常规依赖** |

宪章原文是「只允许 `std` + 自己的 `kevy-*` crate」。`luna-core` 是属主自家的
Lua VM,但**不叫 `kevy-*`**。这不是违规也不是合规 —— 是**宪章措辞没有覆盖
到的情况**,需要属主判定:自家跨项目 crate 算不算「自己的」。

按 [[module-craft]] §12 的分界,这需要价值判断 → **C 层,抽象成选择题递交**。

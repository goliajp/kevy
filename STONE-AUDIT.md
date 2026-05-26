# kevy stone audit — the infra bar

> 这是 kevy 项目内部对一个 crate 是否够格做"stone"的判定标准。**比 mailrs
> 的 [stone-audit.sh](../../goliajp/mailrs/scripts/stone-audit.sh) 更严**，
> 因为 mailrs 是 application（邮件服务器），kevy 是 **infrastructure** —
> 缺陷会被 N 个上层 caller 放大；性能契约一旦发布就难撤回；unsafe 的爆炸半径
> 直接覆盖每个使用者。

定义参照 mailrs `ARCHITECTURE.md` 的 stone / cement 区分：**stone = 单一
identity、generic、可发布、独立可用的基础 crate**。kevy 目前的 stones (8):
`kevy-bytes`, `kevy-hash`, `kevy-madvise`, `kevy-map`, `kevy-resp`,
`kevy-resp-client`, `kevy-ring`, `kevy-uring`. Cement 不走这个 audit，走
`CEMENT-AUDIT.md`（cement-tier 标准；目标是 "do its job in the kevy
server"）。Dev tools (`kevy-bench`, `kevy-pubsub-bench`, `kevy-cli`) 走最
轻的 dev-tool 标准（charter 0-dep 可豁免，但每个引入必须受审计许可）。

**Why kevy-sys is cement (not a stone)**: identity "Hand-curated OS
bindings for kevy — sockets + readiness poller" needs the "for kevy"
qualifier; it's hand-curated to kevy's narrow needs (a third party would
compare against `libc`/`nix`/`rustix`/`mio` and find kevy-sys missing too
much). The two pieces of kevy-sys that *were* generic — the io_uring
engine and the MADV_HUGEPAGE wrapper — were split out as the stones
`kevy-uring` and `kevy-madvise` (see `KEVY-SYS-VERDICT-2026-05-27.md`).
What remains in kevy-sys is the network-boundary cement.

**Why kevy-cli is dev tool (not a stone)**: the "cli" in the name was the
tell — a CLI binary's purpose is to be invoked, not consumed. The
generic protocol pieces were carved out into `kevy-resp-client` (a real
stone); `kevy-cli` itself is now just a redis-cli-style REPL.

每个维度分三层：

- **T1 (GATE)** — 不过就不能叫 stone；publish 之前任何时候 break 都是 P0
- **T2 (BAR)** — kevy 内部使用前的质量门；break 阻止进 develop
- **T3 (PUB)** — crates.io 发布前必须达到；只在发布动作里检查

---

## 1. Identity & API surface

| Layer | Criterion | Why for infra |
|---|---|---|
| **T1** | 一句话 identity（≤ 60 字符）写在 `Cargo.toml::description` | 一句话说不清就是 bag, 不是 stone |
| **T1** | 公开符号（`pub`）每一个都有 rustdoc | infra 的 API 是契约；undocumented = unspecified |
| **T2** | `pub` API surface 列表（grep `^pub ` in lib.rs）≤ stone identity 直接需要的 | 多一个 public method 就是多一个 backwards-compat 锁 |
| **T2** | 一句话之外的能力（"有用但 secondary"）放在 `pub mod extras` 或 feature-gated | identity 不能被 secondary 拖偏 |
| **T3** | crates.io metadata 完整：`description`, `keywords`, `categories`, `license`, `readme = "README.md"` | 发现性 + 法律 |
| **T3** | `CHANGELOG.md` 从 0.1 起逐版本记录 breaking changes | semver 真实性 |

**怎么验**:
```bash
# T1 identity 长度
head -1 <(grep '^description' Cargo.toml) | wc -c
# T1 0 missing-docs
RUSTDOCFLAGS="-D warnings -D rustdoc::missing-doc-code-examples" cargo doc --no-deps -p <name>
# T2 pub surface count
grep -rE '^pub (fn|struct|enum|trait|type|const|union|use)' src/ | wc -l
```

---

## 2. Correctness (baseline)

| Layer | Criterion | Why for infra |
|---|---|---|
| **T1** | `cargo test -p <name>` 100% pass | 不解释 |
| **T1** | `cargo clippy -p <name> --all-targets -- -D warnings` 0 findings | infra 没有"clippy 是建议"这种话术余地 |
| **T1** | `cargo doc --no-deps -p <name>` 0 warnings | broken doc = broken API surface |
| **T1** | 测试覆盖：单元测 + 边界（0/1/max/empty/overflow）+ drop/panic safety | infra 的 happy-path 覆盖不够，bug 只在边缘条件触发 |
| **T2** | Line coverage ≥ 90% (`cargo llvm-cov -p <name>`) | infra 标的，app 80% 不够；coverage gaps 都要有理由 |
| **T2** | 公开 API 的每一个公开方法至少一条 doctest 或 unit test 直接调用 | "这玩意儿真的被 test 用了么" 的最便宜证明 |

**怎么验**:
```bash
cargo test -p <name>
cargo clippy -p <name> --all-targets -- -D warnings
cargo doc --no-deps -p <name>
# coverage (需 cargo-llvm-cov, 0-dep dev tool):
cargo install cargo-llvm-cov  # 一次性
cargo llvm-cov -p <name> --summary-only
```

---

## 3. Correctness (unsafe / cross-thread)

**这是 infra 比 app 加严的最大头**。

| Layer | Criterion | Why for infra |
|---|---|---|
| **T1** | Stone 默认 `#![forbid(unsafe_code)]`；要 unsafe 必须在 crate 顶部 doc 解释 why | scope 默认锁死 |
| **T1** | 每个 `unsafe { ... }` 块上方有 `// SAFETY:` 注释，说明被依赖的 invariant | 不能"按 convention" 写 unsafe；invariant 不文档化 = 不存在 |
| **T2** | `cargo miri test -p <name>` clean（在装了 nightly + miri 的机器上）| undefined behavior 是 infra 最大恐惧；miri 是 first-line 检测 |
| **T2** | 如果跨线程：`loom` 测试至少覆盖一个 happy + race scenario | 单线程 happy path 不够；data race 是 infra debug 噩梦 |
| **T2** | parser / decoder / 协议处理 stones：有 `fuzz/fuzz_targets/*.rs`（cargo-fuzz），跑过 ≥ 1h | parser 是 attack surface；fuzz 是发现 panic / hang / OOM 的捷径 |
| **T3** | miri & fuzz 跑过的 timestamp + 命中 0 issues 记录在 `STONE-STATUS.md` | 发布快照 |

**适用 stones（已有 unsafe）**:
- `kevy-bytes` (union)
- `kevy-map` (MaybeUninit slots + advise_hugepage)
- `kevy-sys` (extern "C" libc)
- `kevy-ring` (lock-free SPSC)

**怎么验**:
```bash
# miri
rustup toolchain install nightly --component miri
cargo +nightly miri test -p <name>
# fuzz (parser stones)
cargo install cargo-fuzz
cd crates/<name> && cargo +nightly fuzz run <target> -- -max_total_time=3600
```

---

## 4. Performance contracts

| Layer | Criterion | Why for infra |
|---|---|---|
| **T1** | 性能契约（"zero-alloc on hot path", "O(1) average", "≤ N cycles"）写在 module 头 doc | 没写 = 没契约；caller 不能 assume |
| **T1** | 每条性能契约对应一个 test 或 const-assert 钉死 | 见 `kevy-bytes::SmallBytes` 的 `assert!(size_of::<SmallBytes>() == 24)` — 这是模板 |
| **T2** | `benches/` 目录有 criterion-style bench（kevy 用自己的 `kevy-bench`）覆盖每个 hot fn | 性能 regression 无 bench 发现不了 |
| **T2** | Baseline bench vs `std` 或显然 competitor（hashbrown, rustc_hash, …）在 BUDGETS.md 记录 | 不跟标比，"快" 是空话 |
| **T2** | `tests/perf_gate.rs` 形式的预算 test：超过阈值 fail | regression 必须自动报警，不能 review-by-eye |
| **T3** | `BUDGETS.md` 列出每个 hot fn 的当前 ns、measured-on、reproducer 命令 | publish 之前必须可复现 |

**怎么验**:
```bash
# const-assert 已在 source；test 命令同 §2
cargo bench -p <name>            # criterion / kevy-bench
cargo test -p <name> --test perf_gate
```

---

## 5. Memory contracts

| Layer | Criterion | Why for infra |
|---|---|---|
| **T1** | `size_of` of pub types 用 `const _: () = { assert!(size_of::<T>() == N); };` 钉死 | layout 偏移直接影响上层 cache 行为 |
| **T1** | "alloc-free hot path" 类承诺有 alloc-count test（hook GlobalAlloc 计数）| caller 信你的话靠这个 |
| **T2** | 大 owned type 的 Drop 测过 leak free（counter pattern）| infra 的 leak = N 个 caller 都 leak |
| **T2** | heap profile（dhat / valgrind massif）on representative ops 跑过 + 数据存档 | bytes-per-op 是 infra advertised number |
| **T3** | `MEM-BUDGETS.md` 列出 per-op heap usage | published number 必须有源 |

**怎么验**:
```bash
# const-assert 在 source
# alloc-count: 写一个 test with a counting GlobalAlloc swap (no third-party dep)
# dhat 需要 nightly + dhat-rs, 是 dev tool, OK 用
```

---

## 6. Cross-platform & cross-arch

| Layer | Criterion | Why for infra |
|---|---|---|
| **T1** | 平台/ABI 假设显式声明（top-of-file `compile_error!` on 不支持的 cfg）| infra 跑在哪都得"挂的明白" |
| **T1** | endian 假设显式：`#[cfg(target_endian = "big")] compile_error!("...")` | LE 假设悄悄 break BE 是 infra 经典坑（kevy-bytes 已做对）|
| **T2** | CI matrix: x86_64 + aarch64（min linux + macos）| infra 是给跨 arch caller 用的 |
| **T2** | atomic ops 用的内存序明确记录 + 解释（SeqCst 不是 default） | weak-mem-model arch 上的 bug 是噩梦 |
| **T3** | `PLATFORMS.md` 列 supported targets + 已知 caveat | 跟 caller 的合同 |

**怎么验**:
```bash
# cross arch local check (macOS only quick: aarch64-apple-darwin native)
cargo check -p <name> --target aarch64-apple-darwin
cargo check -p <name> --target x86_64-unknown-linux-gnu
```

---

## 7. Footprint

| Layer | Criterion | Why for infra |
|---|---|---|
| **T1** | 0 third-party (crates.io) deps；只允许 path-dep 到 其他 kevy stones | kevy charter 锁；infra dep 是夹带传染 |
| **T1** | `unsafe` scope 跟 §3 self-declared scope 一致（grep 不出意外位置）| scope drift = 风险 drift |
| **T2** | LOC ≤ 1000（src/ 下 .rs 总和）| 单 stone 超 1000 LOC 是"≥ 2 个 stone 混在一起"的强信号 |
| **T2** | `cargo package --list -p <name>` 字节 ≤ 50 KB（不含 README/CHANGELOG）| crates.io tarball footprint |
| **T3** | binary 影响 measurement (`size -A`): stone 对 final binary 贡献的 .text 字节有 budget | infra 加 binary 字节是 N caller 都付 |

**怎么验**:
```bash
find crates/<name>/src -name '*.rs' -exec cat {} + | wc -l
grep -rE '(^|[^a-z_])unsafe(\s|\{)' crates/<name>/src/
cargo package --list -p <name>
```

---

## 8. Charter alignment (kevy-specific)

| Layer | Criterion | Why |
|---|---|---|
| **T1** | `[dependencies]` 段无任何 crates.io entry | charter L2 锁定 |
| **T1** | libc / OS syscall 只在 OS-boundary 系列 crates (`kevy-sys`, `kevy-uring`, `kevy-madvise`)；其他 stone 不 `extern "C"` | charter; OS boundary 集中可审 |
| **T1** | `Cargo.toml`: `version.workspace`, `edition.workspace`, `authors.workspace` 全部 inherit | 不允许 stone 偏离 workspace policy |
| **T2** | 跨 stone 的 dep 单向（grep dep graph 无 cycle）| infra cycle = 编译/包管理梦魇 |

**怎么验**:
```bash
# charter dep:
awk '/^\[dependencies\]/{flag=1;next} /^\[/{flag=0} flag' crates/<name>/Cargo.toml | grep -v '^kevy-\|^$'
# libc 边界:
grep -l 'extern "C"' crates/*/src/*.rs | grep -v 'crates/kevy-sys'
```

---

## Audit verdict format

每次跑 audit 在 `crates/<name>/AUDIT-<DATE>.md` 写：

```markdown
# <name> audit — YYYY-MM-DD

## T1 (GATE)
- [x] identity: "..."  (52 chars)
- [x] missing-docs: 0
- [x] cargo test: pass
- [x] clippy -Dwarnings: 0
- [x] cargo doc: 0 warnings
- [x] unsafe SAFETY comments: all 12 blocks
- [x] 0 third-party deps
- [ ] (any T1 fail blocks promotion)

## T2 (BAR)
- [x] coverage ≥ 90% (got 94%)
- [ ] miri: NOT RUN (no nightly here) — DEFERRED, see followup
- [x] benches/: 3 fns covered
- [x] perf gate test: pass
- [x] LOC: 459 (under 1000)
- [ ] (T2 fail blocks merging this stone version)

## T3 (PUB)
- [ ] CHANGELOG: TODO
- [ ] PLATFORMS.md: TODO
- [ ] MEM-BUDGETS.md: TODO
- (T3 only required at publish time)

## Verdict
GATE: ✅ pass; BAR: ⚠️ miri deferred; PUB: ⏸ not yet
```

---

## 跟 mailrs 6-dim audit 的差异 + 理由

| mailrs (app) | kevy (infra) | 为什么加严 |
|---|---|---|
| doc: 0 warnings | + missing-docs 0 + 公开 API doctest 覆盖 | app 用户读 doc 偶尔；infra caller 读 doc 写代码 |
| test: cov % | 90% line cov + 边界 + drop/panic safety + alloc-count tests | infra bug 倍乘 |
| bench: criterion --quick | + baseline-vs-std + perf_gate test + 性能契约 const-assert | "快" 没基线对比是空话 |
| size: cargo package --list | + LOC ≤1000 + binary 贡献 budget + size_of asserts | publish 是一面，运行时 footprint 是另一面 |
| perf: bench median number | （同 bench 维）+ ns/op 记录 + reproducer | infra 性能 number 是 published contract |
| mem: dhat flag | + alloc-count test + size_of asserts + MEM-BUDGETS | infra 的"alloc-free"承诺必须可测 |
| ~ | + miri / loom / fuzz (T2)  | infra unsafe 的爆炸半径不能靠人眼 review |
| ~ | + endian / arch 显式声明 + cross-arch CI | infra 是给所有 arch caller 用的 |
| ~ | + charter alignment (0 deps, libc 只在 kevy-sys) | kevy 项目级 hard constraint |

---

## Next action

1. 跑这个 audit on **kevy-bytes** 和 **kevy-map**（最年轻的两 stones）, 列 T1/T2/T3 缺口
2. 按缺口排序补：先所有 stones 的 T1，再 T2，T3 push 到 publish 前
3. 装 `cargo-llvm-cov` (charter-OK dev tool) 跑 coverage 数字
4. miri 需要 nightly toolchain → 装上跑 unsafe stones

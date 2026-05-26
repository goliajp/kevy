# kevy-sys 架构判定 — 2026-05-27

## 问题

v0.publish 准备阶段，发现 stone `kevy-map`（已通过 T1+T2+T3）依赖
cement `kevy-sys`，导致 `kevy-map` 不可独立发布。用户指示：
"是不是 stone 是有硬标准的（不是简单决策题）；如果有 cement+stone
就不符合架构原则要拆或改"。本文是按项目硬标准做的判定。

## 适用规则

**STONE-AUDIT.md §1 顶部**
> stone = 单一 identity、generic、可发布、独立可用的基础 crate

**STONE-AUDIT.md §7 T2**
> LOC ≤ 1000（src/ 下 .rs 总和）。单 stone 超 1000 LOC 是"≥ 2 个 stone
> 混在一起"的强信号

**CEMENT-AUDIT.md §3 T2（关键）**
> Identity attempt: write one sentence describing what this cement does.
> If the sentence comes out clean + generic, suspect a hidden stone —
> split it out (this is how kevy-resp-client got carved out of kevy-cli).

也就是说，cement 内部如果有 clean+generic 内容，**项目规则本身就要求 split**。
这是判定 kevy-sys 的硬标准入口。

## 现状对照

`kevy-sys` 当前 1748 LOC（lib.rs 866 + uring.rs 882）— 远超 stone §7 T2
1000 LOC 上限的"两个 stone 混装"强信号。内容拆解：

| 块 | LOC | identity（一句话） | clean+generic? | 当前被谁用 |
|---|---|---|---|---|
| sockets + Poller(epoll/kqueue) + Waker + sockaddr | ~830 | "Hand-curated OS bindings for kevy network server" | ❌ 需要 "for kevy" 限定 | kevy-rt (cement), kevy bin (cement) |
| **io_uring engine** | 882 | "Pure-Rust io_uring bindings against the kernel ABI (no liburing)" | ✅ 完全 generic | kevy-rt (cement) |
| **advise_hugepage** | ~38 | "Pure-Rust MADV_HUGEPAGE hint" | ✅ 完全 generic | **kevy-map (stone)** |

stone `kevy-map` → cement `kevy-sys` 的依赖只用了 `advise_hugepage` 一个
函数（38 LOC）— 这是让 stone 沾 cement 的全部原因。

## Verdict

**违反 CEMENT-AUDIT §3 T2，需要 split。**

按"硬标准 → 内容是否 clean+generic"逐块判定：

- `kevy-sys` 整体身份合规（"for kevy" 限定明确）。
- 但内部装了两块明显 clean+generic 的内容：`io_uring` 和 `advise_hugepage`。
  按 CEMENT-AUDIT §3 T2，这是 "hidden stones"，必须 split。
- 不 split 的代价：(a) kevy-map 不可独立发布；(b) kevy-sys 1748 LOC 是
  stone §7 T2 红线两倍；(c) 把 io_uring engine 当 cement 内容是误分类。

## 拆分方案

| 新 crate | 类型 | LOC（估） | description (≤60 char) |
|---|---|---|---|
| **kevy-madvise** | stone (新) | ~50 | `Pure-Rust madvise hints (MADV_HUGEPAGE) — no libc crate.` |
| **kevy-uring** | stone (新) | ~882 | `Pure-Rust io_uring bindings against the kernel ABI — no liburing.` |
| **kevy-sys** | cement (缩) | ~830 | `Hand-curated OS bindings for kevy — sockets + readiness poller.` |

dep 关系变更：

```
旧:                     新:
  kevy-map → kevy-sys     kevy-map → kevy-madvise   (stone → stone ✓)
  kevy-rt  → kevy-sys     kevy-rt  → kevy-sys       (cement → cement ✓)
                          kevy-rt  → kevy-uring     (cement → stone, OK)
  kevy     → kevy-sys     kevy     → kevy-sys       (cement → cement ✓)
```

## Charter 同步修订

charter（memory `[[feedback-pure-rust-no-c-principle]]` + STONE-AUDIT §8 T1
+ CEMENT-AUDIT §5）原文 "libc / extern C 只在 kevy-sys"，**这一条要改**：

> libc / extern C 集中在 **OS-boundary crates** 系列：当前 `kevy-sys`
> （sockets+poller, cement）、`kevy-uring`（io_uring, stone）、
> `kevy-madvise`（madvise, stone）。新加 OS-boundary 必须独立 crate 并
> 写入这个列表。其他 stone/cement 仍不允许 `extern "C"`。

**精神不变**（libc 集中、外部不 extern C），文字从"一个 crate"放宽到
"boundary crate 系列"。这是用户提的"拆或**改**"的"改"。

## 发布链（v0.publish）

bottom-up：

1. `kevy-bench` (dev tool, charter-exempt)
2. `kevy-hash`, `kevy-bytes`, `kevy-resp`, `kevy-ring`, **`kevy-madvise`**, **`kevy-uring`** (stones, no inter-stone deps among these)
3. `kevy-resp-client` (stone, deps `kevy-resp`)
4. **`kevy-map`** (stone, deps `kevy-hash` + **`kevy-madvise`**) — 现在可发了
5. cements + bin: 不发布

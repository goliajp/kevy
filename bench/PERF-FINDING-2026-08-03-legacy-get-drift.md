# legacy_8sh_get 贴线漂移 — 轮廓否证阶梯回退,轮间方差主导(负结果)

## 起因

2026-08 的 v5 T 系列各轮 perfgate 中,legacy_8sh_get 的 cand-vs-ref
(interleaved,ref = baseline commit 4916f7eb)反复出现 -1% ~ -8%,
一次触发定向重测(n=5 median -7.8% 压线过)、一次首轮 FAIL。
interleave 设计使盒况漂移在 cand/ref 间抵消 —— 差值稳定为负即真回退
嫌疑,遂立案查证。

## 三点轮廓(PERFGATE_ANGLES=legacy_8sh_get, INSTANCES=5, lx64)

| commit | 语义位置 | cand vs ref |
|---|---|---|
| e3e0ed28 | T-row 完成 + malloc_trim | -3.3% |
| 5fcc8ed7 | T-text-b 收口 | -5.2% |
| HEAD (e7c7a071) | R4a-time 后 | -2.1% |

同日早间另一独立 n=5 轮:HEAD -7.8%;单角 n=3 轮:-3.3%。

## 判定

1. **无阶梯**:三点覆盖 v5 主线全部 341 commits,相互差 < 1.5pp,
   HEAD 反而最好 —— 不存在"某 commit 引入 X%"的结构。
2. **轮间方差 ≥ gap**:同一 binary 对在独立轮上给 -7.8% 与 -2.1%
   (Δ5.7pp),报告 gap 均值 ~-4%。按 perf-vs-foss §1
   "single run shows -X% loss" 反模式与 Pre-Phase-A gate:
   该 gap 目前无法与测量方差区分,**不满足启动 attack 的条件**。
3. bisect 在此方差下每步判定不可靠(需 n≳15/side 才能分辨 3pp),
   成本远超一个未确认的 ~3% 信号 —— 停。

## 残留信号(诚实记账)

均值确为负(六个独立测量全部 cand < ref)。若为真,~-3% 的候选解释:
二进制布局(v5 后 bin 大了 ~15%,icache/对齐)、fixed-key 角度对
分配器状态敏感。**再启条件**:任一未来轮 n≥5 median < -8%(过容差),
或连续三轮 median < -5%。届时先做 n=15 定向对测,过了方差检验再
decomposition,不 polish。

## 顺产:三个 gate 调用陷阱(已修/已记)

1. perfgate.sh 拷贝异地调用时 `HERE=$(dirname $0)` 找不到 baseline
   → REFUSED(driver 必须用 repo 内的脚本)。
2. build 满载后立即起 gate 撞 idle 预检(driver 加 sleep)。
3. **preflight 残留扫描 `pgrep -f "kevy"` 匹配 "kevybench" 用户名**:
   任何 cmdline 含 `sudo -u kevybench` 的外层驱动都被误判为残留
   → 本次修:排除项加 sudo 包装行(perfgate.sh)。

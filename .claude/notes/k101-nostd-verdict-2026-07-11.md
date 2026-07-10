# K-101 判决书:no_std core = FEASIBLE(2026-07-11,规则判定无条件通过)

规则:std-only ≤12 且无线程/文件硬依赖 → 可行。实测 6 项,全部可注入/可上抛。

- 闭包校正:core 档 = hash+bytes+map+store+**madvise**(map 拖入;其本就 no_std 形态,非 Linux 塌缩 no-op)。
- 6 项 std-only:Instant/OnceLock/SystemTime(时钟——注入面已有 wasm 先例)、HashMap×2(→ 自研 kevy-map)、mpsc::Sender(bio-drop 可选句柄,闭包内不 spawn)、is_x86_feature_detected(非 x86 臂消失)。
- 实施形锁定:cfg(feature="std") 双态;no_std = --no-default-features --features alloc,external-clock;std 路径 byte-for-byte 不变。
- 附带:若 core 档裁掉 stream/zset,std-only 降至 ~4。
- K-704 执行参数即本判决书;详细证据 file:line 见 spike 报告(agent 输出存档)。

# kevy — 项目约定

## 沟通语言

**永远用中文沟通。** 所有回复、解释、提问都用中文(代码、标识符、commit message 等技术内容按惯例用英文)。

## 核心约束(已在 L2 锁定,不要擅自更改)

- **纯 Rust + 0 依赖**:`Cargo.toml` 里**不得有任何 crates.io 第三方依赖**,只允许 `std` + 自己的 `kevy-*` crate。
- **不许为算法/数据结构 FFI 找 C**:hashmap、分配、hash、协议解析、reactor 逻辑等全部纯 Rust 自研;**唯一允许的 libc 是完全无法避免的 OS 边界**(socket / poller / mmap / time),且只集中在 `kevy-sys` 里用 `unsafe extern "C"` 手写绑定(不引 `libc` crate)。详见 memory `feedback-pure-rust-no-c-principle`。
- **工具链**:Rust 2024 edition,rust-version 1.95。**author = GOLIA K.K.**(workspace 继承)。
- **crate 命名**:一律 `kevy-` 前缀,每个尽量做成可复用的 infra lib。
- **性能目标**:对标并远超 valkey 9.1。基准方法见 `bench/REPORT.md`。

## 规划方法

按用户的 4 层信息架构(L1 roadmap / L2 版本边界 / L3a hot 计划带检测命令 / L3b cold backlog / L4 trigger)推进。checkpoint 状态见 memory `project-kevy-roadmap-state`。当前用户授权 **autorun**(自主推进 checkpoint,完成时报告进度,不必逐步等批准)。

## 常用命令

- 测试:`cargo test --workspace`
- 跑 server:`cargo run -p kevy --bin kevy -- --port 6004`(默认 bind 127.0.0.1;容器内用 `KEVY_BIND=0.0.0.0`)
- 基准对打 valkey:`bash bench/run.sh`
- 开发端口:6004(port-registry 已登记)

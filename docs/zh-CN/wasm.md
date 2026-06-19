# kevy 在 WebAssembly 上

`kevy-embedded`(kevy 的进程内变体 —— 见
[`crates/kevy-embedded/README.md`](../../crates/kevy-embedded/README.md))
在 WebAssembly 上**可编译且可运行**。**完整的内存 KV —— 含 TTL/过期 —
现今**在 `wasm32-unknown-unknown` 上工作(`set` / `get` / `del` /
`set_with_ttl` / `pttl` / reaper `tick`,在 Node 端 end-to-end 验证过;见
[`examples/wasm-kv/`](../../examples/wasm-kv))。完整的 `kevy` 服务器
(`kevy-rt`、`kevy-sys`)**不**面向 wasm —— 它需要 socket、thread、
WASM runtime 不暴露的 OS poller。

> ℹ️ **`wasm32-unknown-unknown` 上需 host 喂时钟**。该 target 没有
> `Instant`/`SystemTime`(调用会 trap `unreachable`),所以 kevy 的时
> 钟 cfg-gate 到一个 **host-fed** 来源:embedding 通过
> [`kevy_embedded::set_clock_ns`] 推进时间(单调 ns,例如
> `Date.now() * 1e6`)—— 用到 `XADD` auto-ID / `EXPIREAT` 时还需要
> [`set_wall_clock_ms`]。在 TTL-敏感操作之前以及每 `tick` 喂一次,
> TTL / 过期 / `DEL` 全部工作。(在原生 target 和 WASI
> `wasm32-wasip1` 上直接用 OS 时钟,无需喂入。)**早期版本的 kevy 在
> 这里每条 TTL 操作和 `DEL` 都会 trap,在 clock port 落地之前。**

显式支持三个 WASM runtime:

| Runtime | Target triple | 线程 | 持久化 | 用途 |
|---------|--------------|----|------|------|
| 浏览器 | `wasm32-unknown-unknown` | 无 | 仅内存 | 客户端缓存、JS 互操作 |
| WASI | `wasm32-wasip1` | 无 | 是(preopened dirs) | wasmtime、wasmer、服务端 WASI host |
| Cloudflare Workers | `wasm32-unknown-unknown`(配 Workers shim) | 无 | KV-binding 桥接(本文不涉及) | 边缘缓存 |

## 编译检查

```bash
# 浏览器风格 WASM(此处不带 JS bindings,user 自己接)
cargo check --target wasm32-unknown-unknown -p kevy-embedded

# WASI(通过 preopened directories 上的 std::fs 实现文件系统持久化)
cargo check --target wasm32-wasip1 -p kevy-embedded
```

两个在 v1.0 代码上都成功。

## 必需配置

### 浏览器风格 wasm32 上 TTL reaper 必须 `Manual`

`wasm32-unknown-unknown` 没有线程派生 runtime,所以默认的
`TtlReaperMode::Background`(它调 `std::thread::Builder::spawn`)会失
败 —— 用 manual reaper 打开:

```rust
use kevy_embedded::{Config, Store};

let s = Store::open(Config::default().with_ttl_reaper_manual())?;
```

### 在 TTL 操作前和每 tick 喂 host 时钟

`wasm32-unknown-unknown` 上,从 host 推进 kevy 的时钟,然后驱动 manual
reaper。一个典型的 JS 端循环(用
[`examples/wasm-kv/`](../../examples/wasm-kv) 里的 `wasm-bindgen`
wrapper):

```js
setInterval(() => { cache.set_clock(Date.now()); cache.tick(); }, 100);
```

…wrapper 转发到 wasm-only setter:

```rust
use kevy_embedded::{set_clock_ns, set_wall_clock_ms};

// ms = Date.now(); 在 TTL 敏感 op 前,以及每 tick 调一次。
set_clock_ns(ms.saturating_mul(1_000_000)); // 单调 deadline 时钟
set_wall_clock_ms(ms);                       // 墙钟(XADD/EXPIREAT)
store.tick();                                // 主动 reaper sweep
```

在 host 喂值之前时钟读 `0`,所以 key 看起来活着且永不早过期 —— 这是安
全方向。(WASI `wasm32-wasip1` 有可用的 `Instant` 和 `SystemTime`,所
以那里不需要喂。)

### WASI 持久化需要 preopened 目录

`std::fs::File::create` 和同类只在 `wasm32-wasip1` 上工作,**当且仅当**
host 已通过 `--dir`(或等价 runtime API)授予 WASM module 对某目录的访
问。把持久化路径串进 `Config::with_persist`,确保 runtime 启动也授权它:

```bash
wasmtime --dir=/data myapp.wasm
```

Rust 内:

```rust
let s = Store::open(
    Config::default()
        .with_persist("/data")
        .with_ttl_reaper_manual()
)?;
```

像 wasmtime、wasmer 这样的 WASI shell 会把 `/data` 读写路由到你映射
的 host 目录。

### Cloudflare Workers

Workers 在 `wasm32-unknown-unknown` 风格的沙箱里跑 WASM,没有直接的文
件访问。用 kevy-embedded 的纯内存模式,把持久化通过 JS 端的平台 KV
bindings 路由出去。`Store::log(...)` 这个 escape hatch 让你把每次写
mirror 到自定义 sink —— 用 Workers KV 写来实现外部 "AOF",让
kevy-embedded 负责内存状态。

## WASM 上**不**工作的东西

| 功能 | 原因 | 变通 |
|------|------|------|
| `kevy::serve()`(TCP 服务器) | wasm32 没有 socket | 用 kevy-embedded 进程内 |
| `wasm32-unknown-unknown` 上的 `TtlReaperMode::Background` | 无线程 runtime | 用 `with_ttl_reaper_manual()` + 从 host 事件循环驱动 `tick()` |
| `wasm32-unknown-unknown` 上自走的时钟 | 无 `Instant`/`SystemTime`(它们会 trap) | host 通过 `set_clock_ns` / `set_wall_clock_ms` 喂入;然后 TTL/过期/`DEL` 都工作(WASI `wasm32-wasip1` 无需喂入) |
| 浏览器 wasm32 上的 AOF | 无文件系统 | 纯内存 `Config::default()` |
| 浏览器 wasm32 上的 BGREWRITEAOF | 无 AOF | n/a |
| KV-backed Workers 上的原子 `rename(2)` 语义 | KV 是最终一致 | snapshot 序列化在 JS 层处理 |

## 依赖说明

`kevy-embedded` 自身 ship 零 crates.io 依赖。浏览器 / Cloudflare 集成
需要 `wasm-bindgen`(浏览器 DOM 互操作)或 `worker`(Cloudflare)——
那是应用级依赖,**不**是 kevy-embedded 的,你在下游 crate 里自己接。
我们刻意没有 ship 一个 `examples/wasm-browser`,让仓库内 crate 保持零
依赖;用户基于公共的 `kevy_embedded::Store` API 构建自己的浏览器桥接。

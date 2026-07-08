# kevy 在 WebAssembly 上

`kevy-embedded` 及其依赖闭包可以编译到 WebAssembly，同一份进程内 KV 引擎因此能跑在浏览器、边缘运行时和 WASI 宿主里。

## 什么时候需要它

- **浏览器内 KV**——web 应用里的高速进程内 KV 缓存，接口面和服务端用的一模一样。
- **Cloudflare Workers**（以及类似的边缘运行时）——隔离体内的热缓存，挡在平台提供的耐久 store 前面。
- **嵌入式 WASM 缓存**——更大宿主（游戏引擎、脚本宿主、无服务器容器）里的沙箱插件，想要一个 Redis 形态的 store，又不想拖进整个网络栈。
- **服务端 WASI 插件**——`wasmtime` / `wasmer` 下常驻的 `wasm32-wasip1` 模块，需要持久化到宿主文件系统。

## 核心思路

还是同一份引擎，只拿掉两样东西：OS 时钟和 OS 线程。`kevy-embedded` 拉入 `kevy-store`、`kevy-persist`、`kevy-hash`、`kevy-bytes`、`kevy-map`、`kevy-resp`——全部都能为 `wasm32-unknown-unknown` 和 `wasm32-wasip1` 构建。网络 reactor 相关的 crate（`kevy-rt`、`kevy-sys`、`kevy-uring`）刻意不放进这份闭包，所以 WASM 构建是干净的。原本会 spawn TTL reaper 线程的位置，引擎改成暴露一个 `Store::tick()`，由你从宿主事件循环里调用；在没有线程的浏览器目标上，它读取宿主喂进来的时钟。数据结构、命令、持久化格式全都不变。

## 实例

```rust
use kevy_embedded::{Config, Store, set_clock_ns, set_wall_clock_ms};

// 1. Open with the manual reaper so we don't try to spawn a thread.
let store = Store::open(Config::default().with_ttl_reaper_manual())?;

// 2. Use the engine. On wasm32-unknown-unknown feed the clock first;
//    on wasm32-wasip1 and native it's read from the OS for you.
set_clock_ns(now_ms_from_host().saturating_mul(1_000_000));
set_wall_clock_ms(now_ms_from_host());

store.set(b"hello", b"world")?;
let v = store.get(b"hello")?;            // Some(b"world".to_vec())
store.set_with_ttl(b"flash", b"x", std::time::Duration::from_millis(500))?;

// 3. Drive eviction from the host loop. On the web you'd schedule this
//    with setInterval / requestAnimationFrame; under WASI it's a plain
//    sleep loop.
loop {
    set_clock_ns(now_ms_from_host().saturating_mul(1_000_000));
    set_wall_clock_ms(now_ms_from_host());
    let _stats = store.tick();           // expires due keys
    host_sleep_ms(100);
}
```

宿主侧的粘合代码很少：浏览器上是一句 JS `setInterval(() => { mod.tick(now()); }, 100)`，WASI 下就是普通的 `std::thread::sleep` 循环。其余一切——`set`、`get`、`del`、hash、list、sorted set、脚本、AOF——走的都是你在 Linux 上发布的同一条代码路径。

## 构建矩阵

| 目标 | Cargo 命令 | 备注 |
|---|---|---|
| `wasm32-unknown-unknown`（浏览器） | `cargo build --target wasm32-unknown-unknown -p kevy-embedded` | 没有线程。没有 `Instant` / `SystemTime`——由宿主通过 [`set_clock_ns`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-store/src/lib.rs) 和 [`set_wall_clock_ms`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-store/src/lib.rs) 喂时钟。持久化落在内存目录里。 |
| `wasm32-unknown-unknown`（Cloudflare Workers） | `cargo build --target wasm32-unknown-unknown -p kevy-embedded` | 同一份模块；时钟源用 Workers 运行时的 `Date.now()`。耐久持久化由 JS 一侧通过 Workers KV 绑定完成。 |
| `wasm32-wasip1`（服务端 WASI） | `cargo build --target wasm32-wasip1 -p kevy-embedded` | 依然没有线程，但 `Instant` 和 `SystemTime` 可用，不需要宿主喂时钟。`std::fs` 对预打开的目录有效（`wasmtime --dir=/data`）。 |
| 原生（`x86_64-*`、`aarch64-*`） | `cargo build -p kevy-embedded` | 供参照：默认 spawn 后台 reaper 线程；不需要手动驱动。 |

依赖闭包见 [`crates/kevy-embedded/Cargo.toml`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/Cargo.toml)，re-export 见 [`crates/kevy-embedded/src/lib.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/src/lib.rs)。

## 与原生的差异

| 关注点 | 原生 | WASM |
|---|---|---|
| TTL reaper | 后台线程，自动 spawn | 手动：`Config::with_ttl_reaper_manual()` + 宿主调用 `Store::tick()` |
| 时钟 | OS 的 `Instant` / `SystemTime` | `wasm32-wasip1`：走 OS。`wasm32-unknown-unknown`：宿主通过 `set_clock_ns` / `set_wall_clock_ms` 喂入 |
| 网络服务器 | `kevy-rt` + `kevy-sys` + `kevy-uring` 监听 TCP | 这些 crate 都不在 WASM 构建闭包里；直接通过 `Store` 嵌入 |
| 持久化 | AOF 写入传给 `with_persist` 的目录 | `wasm32-wasip1`：相同，落到预打开的宿主目录。`wasm32-unknown-unknown`：只有内存目录（要耐久就由宿主把写镜像出去） |
| 异步运行时 | 用户代码里的 Tokio / std 线程 | 宿主给什么用什么（JS 事件循环、Workers fetch handler、WASI 单线程循环） |

## 取舍

- **TTL 精度跟着循环节拍走。**TTL 为 500 ms 的键，要等截止时间之后的下一次 `tick()` 才会过期。100 ms 的循环很典型；更紧可以，缓存类用途更松也可以，但引擎不可能比宿主给的节拍更准。
- **不捆绑异步运行时。**kevy-embedded 不拉 `tokio` 或 `wasm-bindgen-futures`。循环归宿主所有；库暴露的是微秒级完成的同步方法。
- **没有后台工作，就没有意外、没有隐藏成本**——但反过来，忘了调 `tick()`，过期键就会一直留着，内存越撑越大。把这个调用和你其他的周期性任务挂在同一个地方。
- **`wasm32-unknown-unknown` 的耐久性不是自动的。**没有文件系统，要么当纯内存缓存用，要么把写镜像到宿主侧的 sink（Workers KV、IndexedDB 等）。

## FAQ

**在浏览器里能用吗？**能。为 `wasm32-unknown-unknown` 构建，用 `wasm-bindgen` 之类的绑定发布生成的 `.wasm`，以 `Config::default().with_ttl_reaper_manual()` 打开，每次 `tick()` 之前从 `Date.now()` 喂时钟。完整的命令面——字符串、hash、list、set、sorted set、pub/sub、脚本——都能在进程内使用。

**Cloudflare Workers 最小可用的搭法？**把 `kevy-embedded` 编译到 `wasm32-unknown-unknown`，每个隔离体实例化一个 `Store`，`tick()` 要么按需调（TTL 敏感的读之前），要么放进 scheduled handler。时钟源是 Workers 运行时的 `Date.now()`。要跨隔离体重启保住数据，就在 JS handler 里把写镜像到 Workers KV 或 D1；引擎本身留在内存里。

**怎么持久化？**在 `wasm32-wasip1` 下，调用 `Config::with_persist("/data")`，并用 `wasmtime --dir=/data`（或你所用运行时的等价参数）启动模块。AOF 写进预打开的目录，下次打开时回放。`wasm32-unknown-unknown` 下没有文件系统，持久化只能由宿主代劳——通常是把写镜像到平台提供的耐久 store。

**线程呢？启用 Atomics 的 WASM 怎么办？**默认的 WASM 构建单线程运行，与所有实际发布的浏览器类目标一致。如果宿主运行时提供共享内存线程（`wasm32-unknown-unknown` 加 `--target-feature=+atomics,+bulk-memory`，再配一个线程池），`Store` 依然可以安全使用，但后台 reaper 模式照样关闭——手动 `tick()` 模型仍是受支持的路径，你代码里的多个线程可以共享同一个 `Store` 并发调用。

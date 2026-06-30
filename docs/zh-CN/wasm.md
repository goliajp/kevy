# kevy 在 WebAssembly 上

`kevy-embedded` 与它的依赖闭包能编译到 WebAssembly,因此同一份进程内 KV 引擎可以跑在浏览器、边缘运行时与 WASI 宿主里。

## 何时需要

- **浏览器内 KV** —— web 应用里的高速进程内 KV 缓存,接口面与你在服务端用的一致。
- **Cloudflare Workers**(以及类似的边缘运行时)—— 一个隔离体内的热缓存,坐在平台提供的耐久 store 前面。
- **嵌入式 WASM 缓存** —— 更大宿主里(游戏引擎、脚本宿主、无服务容器)的沙箱化插件,需要 Redis 形态的 store 但不愿拖入网络栈。
- **服务端 WASI 插件** —— `wasmtime` / `wasmer` 下的长寿 `wasm32-wasip1` 模块,需要持久化到宿主文件系统。

## 核心思路

同一份引擎,拿掉两样东西:OS 时钟与 OS 线程。`kevy-embedded` 拉入 `kevy-store`、`kevy-persist`、`kevy-hash`、`kevy-bytes`、`kevy-map`、`kevy-resp` —— 它们都能为 `wasm32-unknown-unknown` 与 `wasm32-wasip1` 构建。网络 reactor 相关 crate(`kevy-rt`、`kevy-sys`、`kevy-uring`)是有意不在那份闭包里的,所以 WASM 构建是干净的。引擎在原本会产生 TTL reaper 线程的地方,改为暴露一个 `Store::tick()` 让你从宿主事件循环里调用;在无线程的浏览器目标上,它读取宿主喂进来的时钟。数据结构、命令、持久化格式都保持不变。

## 实际示例

```rust
use kevy_embedded::{Config, Store, set_clock_ns, set_wall_clock_ms};

// 1. 用手动 reaper 打开,这样不会尝试 spawn 线程。
let store = Store::open(Config::default().with_ttl_reaper_manual())?;

// 2. 使用引擎。wasm32-unknown-unknown 上先喂时钟;
//    wasm32-wasip1 与原生上从 OS 自动读取。
set_clock_ns(now_ms_from_host().saturating_mul(1_000_000));
set_wall_clock_ms(now_ms_from_host());

store.set(b"hello", b"world")?;
let v = store.get(b"hello")?;            // Some(b"world".to_vec())
store.set_with_ttl(b"flash", b"x", std::time::Duration::from_millis(500))?;

// 3. 从宿主循环驱动驱逐。在 web 上你用
//    setInterval / requestAnimationFrame 排,WASI 下就是 sleep 循环。
loop {
    set_clock_ns(now_ms_from_host().saturating_mul(1_000_000));
    set_wall_clock_ms(now_ms_from_host());
    let _stats = store.tick();           // 过期到期键
    host_sleep_ms(100);
}
```

宿主侧粘合代码很少:浏览器上是一段 JS `setInterval(() => { mod.tick(now()); }, 100)`,WASI 下就是普通的 `std::thread::sleep` 循环。其它一切 —— `set`、`get`、`del`、hash、list、sorted set、脚本、AOF —— 都走你在 Linux 上发布的同一份代码路径。

## 构建矩阵

| 目标 | Cargo 命令 | 备注 |
|---|---|---|
| `wasm32-unknown-unknown`(浏览器)| `cargo build --target wasm32-unknown-unknown -p kevy-embedded` | 无线程。无 `Instant` / `SystemTime` —— 宿主通过 [`set_clock_ns`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-store/src/lib.rs) 与 [`set_wall_clock_ms`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-store/src/lib.rs) 喂时钟。持久化是内存中的目录。 |
| `wasm32-unknown-unknown`(Cloudflare Workers)| `cargo build --target wasm32-unknown-unknown -p kevy-embedded` | 同一份模块;时钟源用 Workers 运行时的 `Date.now()`。耐久持久化由 JS 一侧通过 Workers KV 绑定承担。 |
| `wasm32-wasip1`(服务端 WASI)| `cargo build --target wasm32-wasip1 -p kevy-embedded` | 线程仍然没有,但 `Instant` 与 `SystemTime` 可用,因此不需要宿主喂时钟。`std::fs` 对预打开的目录有效(`wasmtime --dir=/data`)。 |
| 原生(`x86_64-*`、`aarch64-*`)| `cargo build -p kevy-embedded` | 作参考:默认 spawn 后台 reaper 线程;无需手动驱动。 |

依赖闭包见 [`crates/kevy-embedded/Cargo.toml`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/Cargo.toml),re-export 见 [`crates/kevy-embedded/src/lib.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/src/lib.rs)。

## 与原生的差异

| 关注点 | 原生 | WASM |
|---|---|---|
| TTL reaper | 后台线程,自动 spawn | 手动:`Config::with_ttl_reaper_manual()` + 宿主调用 `Store::tick()` |
| 时钟 | OS `Instant` / `SystemTime` | `wasm32-wasip1`:OS。`wasm32-unknown-unknown`:宿主喂入 `set_clock_ns` / `set_wall_clock_ms` |
| 网络服务器 | `kevy-rt` + `kevy-sys` + `kevy-uring` 监听 TCP | 这些 crate 都不在 WASM 构建闭包里;通过 `Store` 直接嵌入 |
| 持久化 | 在传给 `with_persist` 的目录里写 AOF | `wasm32-wasip1`:同样,落到预打开的宿主目录。`wasm32-unknown-unknown`:只在内存目录(想要耐久就由宿主把写镜像出去) |
| 异步运行时 | 用户代码里的 Tokio / std 线程 | 宿主给你什么用什么(JS 事件循环、Workers fetch handler、WASI 单线程循环)|

## 取舍

- **TTL 精度跟随你的循环节拍。** 500 ms TTL 的键只在截止时间之后的下一次 `tick()` 时过期。100 ms 的循环很典型;更紧也行,缓存形态用更松也行,但引擎不可能比宿主给的更准。
- **不绑定异步运行时。** kevy-embedded 不拉 `tokio` 或 `wasm-bindgen-futures`。循环由宿主拥有;库暴露的是微秒级完成的同步方法。
- **没有后台工作意味着没有惊喜,也没有隐藏成本**,但这也意味着遗忘 `tick()` 会让过期键继续活着并把内存撑大。把这个调用接在你接其它周期任务的同一个位置上。
- **`wasm32-unknown-unknown` 的耐久不是自动的。** 没有文件系统,你要么作纯内存缓存,要么把写镜像到宿主侧 sink(Workers KV、IndexedDB 等)。

## FAQ

**它在浏览器里能用吗?** 能。为 `wasm32-unknown-unknown` 构建,带 `wasm-bindgen` 或类似绑定发出 `.wasm`,用 `Config::default().with_ttl_reaper_manual()` 打开,然后在每次 `tick()` 之前从 `Date.now()` 喂时钟。全部命令面 —— 字符串、hash、list、set、sorted set、pub/sub、脚本 —— 都在进程内可用。

**Cloudflare Workers —— 最小搭起来是什么?** 把 `kevy-embedded` 编到 `wasm32-unknown-unknown`,每个隔离体实例化一个 `Store`,在 TTL 敏感读之前懒调用 `tick()`,或者从 scheduled handler 里调用。时钟源是 Workers 运行时的 `Date.now()`。跨隔离体重启的耐久,从 JS handler 里把写镜像到 Workers KV 或 D1;引擎自身保持在内存里。

**怎么持久化?** 在 `wasm32-wasip1` 下,调用 `Config::with_persist("/data")` 并以 `wasmtime --dir=/data`(或你运行时的等价物)启动模块。AOF 落到预打开的目录,下次打开时回放。在 `wasm32-unknown-unknown` 下没有文件系统,所以持久化得由宿主中介 —— 通常把写镜像到平台提供的耐久 store。

**线程 —— 启用 Atomics 的 WASM 怎么办?** 默认 WASM 构建跑单线程,这与每个出货的浏览器形态目标一致。如果你的宿主运行时暴露共享内存线程(`wasm32-unknown-unknown` 加 `--target-feature=+atomics,+bulk-memory` 再加一个线程池),`Store` 仍可安全使用,但后台 reaper 模式仍然关闭 —— 手动 `tick()` 模型仍是支持的路径,你代码里的线程可以共享同一个 `Store` 并发地调用。

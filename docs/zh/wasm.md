# kevy 上 WebAssembly

kevy 在浏览器里是一个真正的 store，不是编译期玩具：npm 包 [`@goliajp/kevy`](https://www.npmjs.com/package/@goliajp/kevy) 把引擎（KV + TTL + 计数器 + 扫描 + pub/sub）编译到 `wasm32-unknown-unknown`，配一个手写的 ES module loader，持久化落 OPFS（IndexedDB 兜底），pub/sub 能跨 tab。同一批 crate 也能编译到 `wasm32-wasip1`，所以 Rust API 在 `wasmtime` / `wasmer` 和边缘运行时里同样可用。

在线体验：[kevy.golia.jp 的 demo](https://kevy.golia.jp/demo/) 就是跑在这个模块上的浏览器 REPL——命令行、重载不丢的 OPFS 持久化、跨 tab 的 pub/sub，全程没有后端。

## 快速开始（浏览器）

```sh
npm install @goliajp/kevy
```

```js
import { open } from "@goliajp/kevy";

const db = await open({ persist: { name: "app" } });

db.set("greeting", "hello");
db.set("session", "abc123", { ttlMs: 60_000 });
db.getText("greeting");            // "hello"
db.incrby("visits");               // 1, 2, 3, ...
db.keys("user:*");

// Pub/sub——包括同源的其他 tab:
const off = db.subscribe("events", (payload, channel) => {
  console.log(channel, new TextDecoder().decode(payload));
});
db.publish("events", "hi from this or any other tab");

await db.flush();                  // 耐久性屏障
```

写入以 kevy append-only 日志的形式流进存储，下次以同一个 `persist.name` 调用 `open()` 时重放。整个包共六个文件（打包约 165 KB）：wasm 模块、loader、OPFS worker、手写的 TypeScript 类型，加上常规的 README 和 manifest。边界两侧都是零依赖。

## Loader API

`open(options)` 实例化模块、重放已存日志（开了持久化时），并启动 tick 定时器与跨 tab 桥。选项：

| 选项 | 默认值 | 含义 |
|---|---|---|
| `wasm` | loader 旁边的 `kevy.wasm` | 模块来源：URL、`ArrayBuffer`、`Uint8Array`、`Response`，或已编译的 `WebAssembly.Module` |
| `persist` | `false`（纯内存） | `{ name, backend }`：每个 `name` 一条日志；`backend` = `"auto"`（OPFS，IndexedDB 兜底）、`"opfs"` 或 `"idb"` |
| `broadcast` | `true` | 走 `BroadcastChannel` 的跨 tab pub/sub 桥 |
| `name` | `persist.name` 或 `"kevy"` | 实例名；同时圈定存储文件与广播频道的作用域 |
| `tickMs` | `100` | TTL 清扫 + 事件轮询节奏；`0` 关掉定时器——自己调 `tick()` |

返回的 `Kevy` 句柄（完整签名见 `kevy.d.ts`）：

| 方法 | 语义 |
|---|---|
| `set(key, value, { ttlMs? })` | SET，可选带过期 |
| `get(key)` / `getText(key)` | GET，返回 `Uint8Array` / UTF-8 文本；不存在或已过期时为 `undefined` |
| `del(key)` / `exists(key)` | DEL / EXISTS |
| `expire(key, ttlMs)` / `persist(key)` / `pttl(key)` | PEXPIRE / PERSIST / PTTL（`-1` 无 TTL，`-2` 无 key） |
| `incrby(key, delta?)` | INCRBY，返回新值 |
| `dbsize()` / `flushall()` | DBSIZE / FLUSHALL |
| `keys(pattern?, limit?)` | KEYS，Redis glob，可选上限 |
| `tick()` | 手动执行一次 TTL 清扫 + 事件轮询；返回过期 key 数 |
| `subscribe(channel, cb)` / `psubscribe(pattern, cb)` | SUBSCRIBE / PSUBSCRIBE；都返回退订函数 |
| `publish(channel, payload)` | PUBLISH 给本 tab 订阅者，且（桥开着时）广播到其他同源 tab；返回本地接收者数 |
| `flush()` | 耐久性屏障：把待写帧刷进存储 |
| `compact()` | 把存储重写为在线键空间的紧凑镜像 |
| `close()` | flush、拆掉桥/定时器/存储、释放实例 |

key 和 value 处处接受 `string | Uint8Array | ArrayBuffer`：字符串在边界处做 UTF-8 编码，字节视图直接透传——value 是真正的二进制，不是字符串。

## 绑定的工作方式（没有 wasm-bindgen）

kevy 的零依赖法则延伸到工具链：边界两侧都没有绑定生成器。[`kevy-wasm`](https://docs.rs/kevy-wasm) crate 导出一套扁平的手写 C ABI——30 个 `extern "C"` symbol；`pkg/kevy.js` 是手写 loader，负责 TypedArray 边界、UTF-8 编解码、持久化泵和跨 tab 桥。约定如下：

- **实例**是 `kevy_open` 发出的 `u32` 句柄；其余每个调用都以句柄开头。`0` 永远不是合法句柄。
- **入参字节**以 `(ptr, len)` 对传入，指向由 `kevy_alloc` 取得、用 `kevy_free` 归还的线性内存。
- **出参字节**落在每实例的结果缓冲区里，经 `kevy_out_ptr` / `kevy_out_len` 读取，在同一句柄的下一次调用前有效——调用方应立即拷出。
- **状态码**：`>= 0` 成功，`-1` 操作错误（结果缓冲区里有 UTF-8 消息），`-2` 句柄非法。
- **数字**以 `f64` 过界（时钟、TTL、计数——全部落在 2^53 精确整数范围内）。
- `kevy_abi_version()` 报告 ABI 契约版本，loader 可据此拒绝不匹配的模块。

导出面分四组：core（open/close/alloc/free/clock/tick/结果缓冲区）、KV（`kevy_set`、`kevy_get`、`kevy_del`、`kevy_exists`、`kevy_expire`、`kevy_persist`、`kevy_pttl`、`kevy_incrby`、`kevy_dbsize`、`kevy_flushall`、`kevy_keys`……）、pub/sub（`kevy_subscribe`、`kevy_psubscribe`、`kevy_unsubscribe`、`kevy_publish`、`kevy_poll_events`）和 AOF 泵（`kevy_aof_frames_out`、`kevy_aof_frame_in`、`kevy_aof_dump`）。随包的 loader 若不合你的宿主，ABI 就是受支持的集成点——crate 文档写明了每个 symbol 和打包字节格式。

## 持久化：一条真日志，与 native kevy 字节同格

浏览器没有文件系统，耐久性由宿主中介：开了持久化后，每笔写同时编码出与磁盘上 kevy AOF 相同的 RESP multi-bulk 帧（`kevy-persist` 格式——[persistence.md](persistence.md)）。loader 每个微任务把待写帧泵进存储一次，所以同步的一串写只花一次存储 append。`await db.flush()` 是耐久性屏障；`flush()` resolve 意味着后端已刷到磁盘。

**浏览器 tab 写出的日志能原样在 native kevy 里重放**——同一个 magic 头、同样的帧。把 `.aof` 从 OPFS 拷出来、指给 native embedded store（或服务器），键空间就回来了。反向同样成立：入站泵接受 native 写的日志。损坏的尾巴遵循 native 重放契约——完好前缀被应用、尾巴被丢弃，下一次 compaction 从在线状态重写存储。

两个后端，`backend: "auto"` 下自动挑选：

- **OPFS**（首选）：由一个小型专用 worker 持有的 `FileSystemSyncAccessHandle`——同步句柄只在 worker 里可用。每次 append 都先 flush 再确认。
- **IndexedDB**（兜底）：同一个泵打到 object store 上，服务没有 OPFS 同步句柄的环境（主要是旧版 Safari）。

Compaction 是自动的：追加日志一旦超过 `max(512 KB, 上一镜像的 4 倍)`，loader 就把存储重写为在线键空间的紧凑镜像（浏览器侧的 AOF rewrite）。`compact()` 可在快照或导出前强制执行一次。

**localStorage 有意不做后端**：约 5 MB 的配额、每次写都阻塞主线程的同步 API、只能存 UTF-16 字符串——作为写日志完全不合格。

## 跨 tab pub/sub

`publish()` 经引擎投递给同 tab 订阅者，并且（桥开着时）把帧经每实例一个的 `BroadcastChannel` 广播给所有其他同源 tab，由各 tab 的本地订阅者接收。投递是 **at-most-once、无 backlog**——只有当下开着的 tab 能收到，后开的 tab 不会补收。这与 kevy 服务器 pub/sub 的契约相同（[pubsub.md](pubsub.md)），照一个面写的代码在另一个面上行为一致。

## 时钟、tick 与 TTL

`wasm32-unknown-unknown` 没有线程也没有 OS 时钟，引擎因此用手动 TTL 清扫器 + 宿主喂入的时钟运行：loader 在每个入口调用前把 `Date.now()` 经 ABI 喂进去，并按定时器（默认 100 ms）跑 `tick()`。后果：

- TTL 精度跟随 tick 节奏：500 ms TTL 的 key 在期限后的第一个 tick 过期。在意就把 `tickMs` 调紧，或 `tickMs: 0` 完全自己掌控节奏。
- 两次调用之间不发生任何后台工作——没有隐藏开销；但 `tickMs: 0` 下忘了调 `tick()`，过期 key 会一直活着、事件队列不排空。

## 性能

完整方法论、环境与原始数字见 [bench/WASM-BENCH.md](../../bench/WASM-BENCH.md)（headless Chrome 驱动，3 轮取中位，16 字节 value，1k 与 100k 条两档）。对上 Web 应用实际拥有的那些存储：

| 轴（n=100k） | kevy-wasm | vs IndexedDB | vs localStorage |
|---|---:|---:|---:|
| 点读（ops/s） | 1.67 M | **77×** | 0.48× |
| 点写（ops/s） | 1.79 M | **189×** | 4.9× |
| 批量载入（ms） | 60.8 | **快 36×** | 快 4.5× |
| 扫描，约 10% 命中（ms） | 5.6 | **快 46×** | 快 5.7× |
| 耐久写（ops/s） | 785 k（OPFS） | **12.6–17.4×** | 约 2× |
| 重启到可用（ms） | 129 | **快 2.7×** | 更慢（见下） |

头条与诚实的 caveat：

- **对 IndexedDB 每条轴都是数量级碾压**——点读 77–86×，点写跨数据集档位 166–189×。
- **耐久写是裸 IndexedDB 的 12.6–17.4×**：泵按微任务批量写帧，一串写只花一次存储 append。它同时还比 localStorage 发完就忘的 `setItem` 快约 2 倍——而且是一条真正的 append 日志。
- **localStorage 点读是 kevy 唯一不赢的一列**（0.42–0.48×），bench 里完整拆解了为什么任何 wasm 宿主的 store 都赢不了：Chrome 的 localStorage 读是渲染进程内存里的哈希查找（实测 3.5–5 M ops/s，属于裸 `Map` 级别，不是存储级别），而每个 wasm store 每次调用都要付 UTF-8 编码 + 边界穿越 + 从线性内存拷出——约 2–3 M ops/s 的天花板。kevy 在 loader 的 staging 优化（常驻暂存缓冲、`encodeInto`、缓存的 memory 视图——比朴素 loader 提升 2.2×）之后贴着这个天花板跑（1.7–2.1 M）。kevy 赢下的是 localStorage 在任何速度下都做不到的一切：二进制 value、TTL、计数器、扫描、大于 5 MB 的数据集、pub/sub、以及不阻塞的持久化。
- **跨 tab RTT** 在 64 KB 载荷上胜过 storage-event hack（p50 0.295 ms vs 0.335 ms），1 KB 以下与约 0.105 ms 的任务跳底线打平——而 storage-event hack 在慢消费者下会丢帧、只能传字符串、每次 ping 都写进磁盘配额。

## Rust 面：WASI、边缘运行时、直接嵌入

引擎本身——`kevy-embedded` 及其依赖闭包（`kevy-store`、`kevy-persist`、`kevy-hash`、`kevy-bytes`、`kevy-map`、`kevy-resp`）——两个 wasm target 都能编译；CI 在每次 push 上把整张清单加 `kevy-wasm` 一起 gate。网络 reactor 三件套（`kevy-rt`、`kevy-sys`、`kevy-uring`）有意留在闭包之外，wasm 构建因此保持干净。

| Target | 命令 | 说明 |
|---|---|---|
| `wasm32-unknown-unknown` | `cargo build -p kevy-wasm --target wasm32-unknown-unknown --release` | 浏览器工件（`kevy_wasm.wasm`）；npm loader 实例化它。自己的宿主直接驱动 C ABI。 |
| `wasm32-unknown-unknown`（Rust API） | `cargo build -p kevy-embedded --target wasm32-unknown-unknown` | 无线程、无 OS 时钟：用 `Config::with_ttl_reaper_manual()` 打开，喂 `set_clock_ns` / `set_wall_clock_ms`，宿主循环里调 `Store::tick()`。 |
| `wasm32-wasip1` | `cargo build -p kevy-embedded --target wasm32-wasip1` | `Instant` / `SystemTime` 可用——不用喂时钟。`std::fs` 对 preopen 目录可用，所以 `Config::with_persist("/data")` + `wasmtime --dir=/data` 给出真 AOF 耐久性。线程仍缺席：继续用手动清扫器。 |

在 `wasm32-unknown-unknown` 上用 Rust 直接嵌入：

```rust
use kevy_embedded::{Config, Store, set_clock_ns, set_wall_clock_ms};

let store = Store::open(Config::default().with_ttl_reaper_manual())?;

// Host feeds the clock before time-sensitive work…
set_clock_ns(now_ms_from_host().saturating_mul(1_000_000));
set_wall_clock_ms(now_ms_from_host());

store.set(b"hello", b"world")?;
let v = store.get(b"hello")?;   // Some(b"world".to_vec())

// …and drives expiry at its own cadence:
let _stats = store.tick();
```

错误是 `KevyResult` / `KevyError`，argv 风格的写方法接受借用切片，与 native 完全一致——见 [`kevy-embedded`](https://docs.rs/kevy-embedded) 文档。

Cloudflare Workers 之类的边缘 isolate 照浏览器配方：每 isolate 一个实例，`Date.now()` 当时钟源，`tick()` 惰性调或挂 scheduled handler。要跨 isolate 重启的耐久性，就在你的 handler 里把写镜像到平台的持久存储（AOF 泵会把帧交给你）；isolate 内部，kevy 是热的内存层。

## FAQ

**完整命令面在浏览器里都可用吗？**npm 包暴露的是 KV + TTL + 计数器 + 扫描 + pub/sub 这一刀——浏览器 store 需要的那部分，刻意保持小巧（wasm 模块未压缩约 425 KB）。wasm target 上的 Rust API 则暴露你编译进来的 `kevy-embedded` feature 的全部能力。

**持久化的数据可移植吗？**可以——它就是标准 kevy AOF。浏览器 → native 和 native → 浏览器都能重放。格式契约见 [persistence.md](persistence.md)。

**共享内存线程（`+atomics`）呢？**随包模块是单线程的，与所有浏览器类 target 匹配。宿主提供线程的地方引擎是线程安全的，但受支持的路径仍是手动 `tick()` 模型。

**为什么不用 SharedWorker 而用 BroadcastChannel？**单一 owner 的 SharedWorker 拓扑理论上更干净，但可用面更窄（Android Chrome 上明显缺席）；BroadcastChannel 桥以同一实测延迟档的兼容性胜出。

**为什么不用 wasm-bindgen？**kevy 的服务器、embedded、浏览器三面都是零第三方依赖。绑定生成器是一个构建期依赖，还会拥有边界布局的所有权；手写 ABI 让契约保持显式、带版本、可审计。

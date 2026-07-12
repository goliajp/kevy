# kevy 跑在 IoT 设备上

`kevy-embedded` 往下缩的力度，和服务器往上扩的力度一样是刻意设计的：一套按 feature 分档的构建，从约 411 KB 的纯内存 KV 内核，一直长到完整的索引 / 复制接口面；面向 Linux 级 IoT 板子（Pi Zero 2、OpenWrt 路由器、工业 ARM）的 musl 交叉构建；以及一个 `no_std` 内核——它在裸机 Cortex-M 目标上是真的被引导起来并跑过的，不只是编译通过。资源预算由 gate 强制执行，不靠 README 承诺。

## Feature 分档

`kevy-embedded` 为每个子系统暴露一个 Cargo feature。默认是全都开；IoT 构建从 `core` 起步，只加设备真正会用的东西：

| Feature | 加进来什么 | 拉进哪些 crate |
|---|---|---|
| `core` | KV + TTL + 计数器 + pub/sub + 原子块 / pipeline 门面 | （基础面——实践中始终开着；单独命名是为了让最小档位能被拼写出来） |
| `persist` | 快照 + AOF 耐久性：`Config::with_persist`、`save_snapshot`、`rewrite_aof`、open 时重放 AOF | `kevy-persist` |
| `index` | 声明式二级索引 + 物化视图 | `kevy-index` |
| `text` | 全文（BM25）索引段 | `index` + `kevy-text` |
| `vector` | ANN / HNSW 向量索引段 | `index` + `kevy-vector` |
| `replicate` | embed 作副本、embed 作写者，以及 CDC feed | `persist` + `kevy-replicate` + `kevy-resp` |
| `listener` | 只读 RESP listener（[embedded-listener.md](embedded-listener.md)） | —— |

档位之间的依赖被编码进 feature 本身：`text` 和 `vector` 蕴含 `index`；`replicate` 蕴含 `persist`（被复制的帧要通过 AOF 的 verb 表重放）。你没选的那些，不只是被链接器丢掉的死代码——它们背后的 crate 压根不会进入构建图。

六个原型档位在每次 push 时都由 CI 做编译门禁，所以工作区往前走的时候，每一种组合都保持可构建：

```text
core                        # sensor cache: RAM-only KV + TTL
core,persist                # + survives power cycles
core,index                  # + declared indexes / views
core,index,text,vector      # + search (BM25, HNSW)
core,persist,replicate      # + edge node feeding a hub
core,listener               # + redis-cli-able diagnostics port
```

写进 `Cargo.toml`：

```toml
[dependencies]
kevy-embedded = { version = "4", default-features = false, features = ["core"] }
```

每一档的 API 面都是同一副：`Store::open(Config)`、`KevyResult` / `KevyError` 错误、写方法收借用切片形式的 argv。照着 `core` 写的代码，等设备日后长出 `persist` 时，原样重新编译即可。

## 资源预算（有 gate，不是愿望）

两条预算线，由 [`bench/iotgate.sh`](https://github.com/goliajp/kevy/blob/develop/bench/iotgate.sh) 当作棘轮强制执行——要抬高其中任何一个数字，都得写一份裁决书：

| 预算 | 线 | 实测 |
|---|---|---|
| 二进制体积（`core` 档消费者，静态 musl） | ≤ 600 KB | **454 KB**（x86_64）· **411 KB**（aarch64）· **383 KB**（armv7） |
| `open` 之后空 store 的 RSS（Linux） | ≤ 2 MB | **336 KB**（aarch64） |

> 这些数字是在 `bench/iot-consumer` 上测的——那是一个刻意留在工作区**之外**的 crate，唯一的依赖就是 `kevy-embedded`。那才是你真正会发布出去的形状。这道 gate 的早期版本量的是工作区里的一个 `--example`；构建 example 会把 dev-dependencies（`kevy` 服务器 crate）拉进来，把上报的体积虚高了约 60%。

`iot` 这个 cargo profile 定义在工作区根部——`release` 的 codegen，但优先级给体积：

```toml
[profile.iot]
inherits = "release"
opt-level = "z"     # size over speed
lto = true          # fat LTO: the linker drops unused subsystems
codegen-units = 1
strip = true
```

复现体积数字：

```sh
cargo build --profile iot -p kevy-embedded --example iot_core \
  --no-default-features --features core
ls -l target/iot/examples/iot_core
```

## 一个完整的传感器缓存

`iot_core` 这个 example 就是大多数设备需要的形状——一个带 TTL 的内存 KV，过期由你手动驱动（除非你主动要，否则没有后台线程）：

```rust
use kevy_embedded::{Config, Store};

fn main() -> kevy_embedded::KevyResult<()> {
    // Manual reaper: no thread is spawned; the device's own loop
    // drives expiry at whatever cadence it already runs.
    let s = Store::open(Config::default().with_ttl_reaper_manual())?;

    s.set(b"sensor:1", b"22.5")?;
    s.set(b"sensor:2", b"3.3")?;
    s.expire(b"sensor:2", core::time::Duration::from_secs(60))?;

    // Call from the main loop / timer ISR bottom half:
    let _expired = s.tick();
    Ok(())
}
```

线程很便宜的板子上，让 reaper 保持默认（一个后台线程）；循环归你自己管的地方，用 `with_ttl_reaper_manual()` + `tick()`。TTL 的精度跟着 tick 的节拍走。

## 交叉编译：musl 与 CI 矩阵

静态 musl 二进制是 Linux 级 IoT 的部署通货——单个文件，不跟 glibc 绑定，`scp` 过去就能跑。

### 平台覆盖——真正跑过的部分

下面每一行都是**构建并执行过**的，不只是编译检查。`core` 档的消费者是在该架构的真实内核上跑起来的；体积是静态 musl 产物的体积。

| 目标 | 板子类别 | 体积 | 状态 |
|---|---|---|---|
| `x86_64-unknown-linux-musl` | 工业 x86、网关 | 454 KB | **跑过** |
| `aarch64-unknown-linux-musl` | Pi Zero 2 / Pi 4-5 / 大多数 ARM64 SBC | 411 KB | **跑过——原生**，空 store RSS 336 KB，286 万 store-ops/s |
| `armv7-unknown-linux-musleabihf` | Pi 2-3、较老的 32 位 ARM | 383 KB | **跑过**（模拟器） |
| `arm-unknown-linux-musleabihf` | Pi Zero / Pi 1（ARMv6） | 411 KB | **跑过**（模拟器） |
| `riscv64gc-unknown-linux-musl` | RISC-V SBC | 420 KB | **跑过**（模拟器）——需要一个 RISC-V 交叉 gcc 当链接器（它的 target spec 要 `libgcc_s`，而 `rust-lld` 供不上），外加 `crt-static` |
| `thumbv7em-none-eabihf` | Cortex-M4/M7 MCU | 145 KB 固件 | **在裸机上跑过**——只有 `kevy-store`，不是完整的 `kevy-embedded` 接口面（那需要 `std`）。见下文。 |

aarch64 那一行的数字才是可信的性能数字：它是在 ARM64 上原生跑的。armv7 / ARMv6 两行证明代码在 32 位 ARM 上是正确的，但它们的耗时和 RSS 带着模拟器开销，不应该被当成设备实测值来读。

所有出货目标都在 CI 里做编译门禁（每次 push 都为每个目标构建那个独立消费者），其中 Tier A 目标还会检查**完整的**默认接口面：

```sh
rustup target add aarch64-unknown-linux-musl
cargo build --profile iot --target aarch64-unknown-linux-musl -p kevy-embedded

rustup target add armv7-unknown-linux-musleabihf
cargo build --profile iot --target armv7-unknown-linux-musleabihf -p kevy-embedded
```

kevy 的零依赖法则在这里得到了回报：没有 C 库要交叉编译，也没有一堆 `-sys` crate 动物园——唯一的 OS 边界是 kevy 自家的 `kevy-sys`，而 `core` 以下的 embedded 闭包连它都不含。

想要的不只是一份编译证明时，在模拟器下跑测试套件是一行的事：

```sh
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER=qemu-aarch64 \
  cargo test -p kevy-embedded --target aarch64-unknown-linux-musl
```

## `no_std` 内核

再往下、彻底离开 Linux，那些存储石头不带 `std` 也能构建。五个 crate 在各自的 `std` 默认 feature 后面藏着 `#![no_std]` 内核——`kevy-store`、`kevy-hash`、`kevy-bytes`、`kevy-map`、`kevy-madvise`。

CI 不只是为 Cortex-M 编译它们：它**在一块 Cortex-M 上把它们引导起来**。`bench/mcu-probe` 是一份裸机固件——手写中断向量表、bump 分配器、panic handler、semihosting I/O，因为 kevy 不接受任何依赖，也就无法去拿 `cortex-m-rt` 或 `embedded-alloc`——它在 QEMU（`mps2-an386`，Cortex-M4）下立起一个真实的 `Store`，并把 KV 与 TTL 接口面跑一遍。它在每次 push 时都跑；store 一旦在 MCU 上行为失常，构建就挂。

```sh
cd bench/mcu-probe && cargo run --release
```

在那块 MCU 上、64 KiB 堆的实测：

| | |
|---|---|
| 固件镜像 | 145 KB |
| 空 `Store` 占的堆 | **0 B**——你不写，store 就一个字节都不分配 |
| 64 个传感器键占的堆 | 17.8 KB |
| TTL | 固件推进自己的时钟时，正确过期 |

**在 MCU 上跑的是 `kevy-store`，不是 `kevy-embedded`。** embedded 门面（`Config`、后台 reaper、持久化、listener）需要 `std`；MCU 拿到的是它底下那台存储引擎，直接驱动。

`no_std` 这一刀在实践中意味着：

- **必须有 `alloc`**——store 是一个堆数据结构；`#[global_allocator]` 由你提供。没有“连 `alloc` 都不要”的档位。
- **`external-clock` 取代 OS 时钟**：宿主通过 `set_clock_ns`（单调）和 `set_wall_clock_ms`（墙钟）喂时间进来，与浏览器构建用的是同一套宿主喂时钟的契约（[wasm.md](wasm.md)）。
- **没有线程、没有文件、没有 socket**——持久化、复制、listener 天然属于 `std` 档；`no_std` 内核就是那台内存引擎。

有一个细节值得知道，因为它正是那种在别处会悄悄坏掉的东西：宿主喂进来的时钟是一个 64 位的单元格，必须能被原子地读到。在有 64 位原子的 ISA 上，它就是一个普通的 `AtomicU64`；在只有 32 位的 MCU 上（ARMv7E-M 没有 64 位原子），它退化成**跨两个 `AtomicU32` 半区的单写者 seqlock**——读者在读到撕裂值时重试，而由于宿主是从单一上下文喂时钟的，重试循环立刻就收敛。从多个上下文并发喂时钟，不在契约范围内。

## 定容指引

- 上面那些数字是 `core` 档——是地板，不是典型值。每项能力实际要多少钱，是在 aarch64-musl 上用 `iot` profile 测的（消费者是真的会**调用**该档位 API 的；一个你从不使用的 feature flag 会被 LTO 消掉，不花钱）：

  | 你用到 | 二进制 | 增量 |
  |---|---|---|
  | KV + TTL（`core`） | 392 KB | —— |
  | + 耐久性（快照 + AOF） | 601 KB | **+209 KB** |
  | + RESP listener | 629 KB | +28 KB |
  | + 二级索引 | 667 KB | +66 KB |
  | + 全文（BM25） | 691 KB | +24 KB |
  | + 向量 ANN（HNSW） | 696 KB | +29 KB |

  耐久性是遥遥领先的最大一笔——其余每项都只是压在它上面的小增量。空 store 的 RSS 也是同样的走势：`core` 档 336 KB，把所有 feature 编进来是 656 KB。所以你选的档位，在常驻内存上体现得比在磁盘字节上还明显。
- 内存随键空间增长：空 store 的 RSS 预算之所以存在，正是为了让“拥有 kevy”这件事本身的基线成本，在你的数据面前小到可以忽略。
- 如果设备需要一个诊断端口，`listener` 给你一个 `redis-cli` 能对话的只读 RESP 端点——见 [embedded-listener.md](embedded-listener.md)——代价是一个线程加一个 socket，写入被结构性地拒绝。
- 闪存上的耐久性：`persist` 写的是和服务器一样的 AOF / 快照格式（[persistence.md](persistence.md)），所以在设备上写下的数据，在机队里任何地方都读得回来。SD 卡这类存储上的 AOF fsync 想要的是 `EverySec`（默认值），不是 `Always`。

## 这不是什么

这里没有任何把**服务器**做小的企图：`kevy`（那个二进制）、`kevy-rt`、`kevy-uring` 之流都假定有一个真实内核，不在 IoT 这一刀的范围内。这里的产品是 embedded 库——你的固件就是那台服务器。

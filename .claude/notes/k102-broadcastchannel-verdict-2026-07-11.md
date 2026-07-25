# K-102 spike report — BroadcastChannel as kevy-wasm cross-tab pubsub channel

Date: 2026-07-10/11 (local)
Environment: macOS (Darwin 25.5.0), Google Chrome 149.0.7827.201, `--headless=new`,
page served over `http://127.0.0.1:<ephemeral>` with COOP/COEP headers
(`crossOriginIsolated === true`, so `performance.now()` resolution = 5 us).

Method: self-contained page `bc_bench.html`; driver `run_headless.py` (Python
stdlib only: local HTTP server + minimal RFC6455 WebSocket client speaking CDP,
`Runtime.evaluate` with `awaitPromise` on `window.__k102promise`).
Two topologies, 3 runs each (all raw JSON in `results-*.json`):

- **iframe mode** — main document + same-origin iframe (separate document/realm,
  same renderer process).
- **dualtab mode** — two REAL browser tabs (echo tab opened first, main page
  opened via CDP `/json/new` as a second tab). This is the topology that matters
  for kevy-wasm.

All numbers below are the **median of 3 runs**; frames are sequential
request/response round trips (RTT) except throughput, which is a one-way blast
measured at the receiver (first-to-last arrival window).

## Numbers

### Round-trip latency (ms), true dual-tab mode

| test                     | n    | p50   | p95   | p99 (worst run) |
|--------------------------|------|-------|-------|-----------------|
| BC RTT, 1 KB payload     | 1000 | 0.115 | 0.175 | 0.46 – 1.16     |
| BC RTT, 64 KB payload    | 200  | 0.225 | 0.490 | 0.77 – 1.40     |
| storage-event RTT, 1 KB  | 1000 | 0.110 | 0.145 | 0.16 – 0.17     |
| storage-event RTT, 64 KB | 100  | 0.420 | 0.480 | 0.58 – 0.66     |

(iframe mode is uniformly a bit faster: BC 1 KB p50 = 0.085 ms / p95 = 0.105 ms;
storage 1 KB p50 = 0.060 ms. Same order of magnitude, so the iframe simulation
is a fair proxy, but dual-tab numbers are the ones to quote.)

### One-way throughput, true dual-tab mode (receiver-side window)

| test                          | frames | frames/s | MB/s   | loss |
|-------------------------------|--------|----------|--------|------|
| BC 1 KB blast                 | 20 000 | ~180 000 | ~175   | 0    |
| BC 64 KB blast                | 1 000  | ~28 600  | ~1 790 | 0    |
| storage-event 1 KB blast      | 2 000  | ~133 000 | ~130   | 0    |

(iframe mode: BC 1 KB ~248 000 fps / 243 MB/s; storage ~194 000 fps.)

### 64 KB payload behavior

No errors, no dropped frames, no clamping in either channel. BC RTT only ~2x
the 1 KB RTT (structured-clone cost scales with size; effective clone+delivery
bandwidth ~1.8 GB/s). storage-event pays ~2x more than BC at 64 KB because the
payload goes through `JSON.stringify`/`JSON.parse` both ways plus UTF-16 string
storage.

## Judgment

**延迟可测,主案可行。**

1. **可测性**:帧延迟可以直接用 `performance.now()` 时间戳差测得,分布干净
   (双 tab p50 0.115 ms / p95 0.175 ms,3 轮间 p50 漂移 < 5 us)。唯一前提:
   非 crossOriginIsolated 上下文里 Chrome 把 `performance.now()` 粗化到 100 us,
   而 BC 单帧延迟本身就在 ~100 us 量级 —— 首轮(无 COOP/COEP)所有延迟都落在
   0/0.1/0.2 格点上,不可用。加 COOP/COEP 头(5 us 分辨率)后分布立即可用。
2. **主案可行**:真双 tab 下 sub-millisecond p99、1 KB 帧 ~18 万帧/s、64 KB 帧
   ~1.8 GB/s、零丢帧 —— 对 kevy-wasm 跨 tab pubsub(哪怕高频 key 失效广播)
   余量在两个数量级以上。
3. **对照组**:storage-event 在 1 KB 小帧上出乎意料地不落下风(RTT 持平,吞吐
   低 ~26%),但 64 KB 时 RTT 差 ~2x,且有结构性缺陷:单 key 覆写语义(慢消费
   者只能看到 newValue,天然会丢中间帧 —— 本测试靠"每写必产生 change 事件"才
   数得全)、写入持久化到磁盘配额(QuotaExceeded 风险)、发起 tab 自己收不到
   事件、payload 必须手工序列化成字符串。BC 是正案,storage-event 只配做
   无 BC 环境的降级路径(如果真需要)。

## 对 T8c 桥实现的工程注意

1. **payload 结构化克隆成本 + 不可 transfer**:`BroadcastChannel.postMessage`
   **没有 transfer list**(与 `postMessage(msg, transferables)` 不同),任何
   ArrayBuffer 都是整体 clone,64 KB 一份大约 35 us(由实测 ~1.8 GB/s 推)。
   桥帧应设计成扁平结构(如 `{v, sender, seq, topic, buf}`,`buf` 用单个
   `Uint8Array`/`ArrayBuffer` 承载编码后的 RESP/值体),避免深层对象图放大
   clone 成本;高频大 value 广播要么做失效通知(只广播 key + 版本号,数据走
   共享存储),要么接受每 tab 一份 copy 的带宽账。另外:测延迟/做帧时间戳要求
   页面 crossOriginIsolated(COOP/COEP),这与 kevy-wasm 未来若用
   SharedArrayBuffer 的前置条件恰好是同一个。
2. **channel 命名约定 + 自环/串扰防护**:BC 按 `(origin, channel name)` 分区,
   同名 channel 的所有同源上下文互通 —— 建议 **每个 kevy 实例一条 channel**
   (命名 `kevy-wasm:<instance-id>:pubsub:v1`,带协议版本号,升级帧格式时换
   `v2` 避免新旧 tab 互相解析失败),topic 放帧内字段做订阅过滤,不要 per-topic
   开 channel(channel 实例数量放大 dispatch 与生命周期管理成本)。注意两点
   语义:(a) 发送方自己的 BC 实例收不到自己的消息,但**同一页面里的其他 BC
   实例会收到**(iframe/worker 同理),帧内必须带 `sender` id 用于去重与防
   自环;(b) 消息只送达当前存活的上下文,无缓冲无重放 —— 新开 tab 需要显式
   状态同步握手,不能指望 channel 里"还有历史"。

## Files

- `bc_bench.html` — 自包含测试页(`?role=echo` 为回声端;`?mode=dualtab` 等外部
  echo tab;默认 iframe 模拟)。真双 tab 手动跑法:HTTP 起服务后先开
  `bc_bench.html?role=echo`,再开 `bc_bench.html?mode=dualtab`(必须 http://
  同源,file:// 下 storage 事件不可靠)。
- `run_headless.py` — headless 驱动;`python3 run_headless.py [--dualtab]`,
  结果打到 stdout 并存 `results.json`。
- `results-run{1,2,3}.json` / `results-dualtab-run{1,2,3}.json` — 6 轮原始数据。

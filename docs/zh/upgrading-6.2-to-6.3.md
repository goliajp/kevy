# 从 6.2 升级到 6.3

一句话版本：**你写过的东西没有一处会失效。** 没有 API 移位，没有 crate 换形状，数据目录双向都能打开，6.2.x 的副本能跟 6.3.0 的主库对话。改个号就完事了。

```toml
kevy-embedded = "6.3.0"
```

这一页余下的部分讲的是 6.3.0 里**多出来了什么**、每一项适合什么场景，以及唯一一处因为原来就是错的而改掉的回复。

## 一句话对照表

| 如果你…… | 要做的事 |
|---|---|
| 跑服务器或某个语言绑定 | 换二进制 / 升包号，别的什么都不用 |
| 从 Rust 嵌入 | 把 `kevy-embedded` 升到 `6.3.0` |
| 用 `HGETALL` 从 hash 里抽样 | 现在可以用 `HRANDFIELD` —— §3 |
| 客户端会在连上时设 `notify-keyspace-events` | 现在能用了；配置文件里的绕行可以删掉 —— §1 |
| 协商 `HELLO 3` **且**按类型解码 | 有五条回复现在带上了真实类型 —— §2 |
| 处理 `GEOPOS` 的错误 | 有一条畸形回复现在是干净的错误了 —— §4 |
| 拉 `ghcr.io/goliajp/kevy:latest` | 重新拉一次；这个 tag 现在指向 6.3.0 |

---

## 1. `CONFIG SET notify-keyspace-events` 真的通到引擎了

**场景。** 你在用一个订阅键过期或 keyspace 事件的库——Spring Data Redis 的 `RedisKeyExpirationEvent`、socket.io 的 Redis adapter、若干任务队列——或者你想要过期/驱逐通知，而因为线上设不进去，只好写在配置文件里。

**原来错在哪。** keyspace 通知很早就能用了，但只能从配置文件走，那里的键名是带下划线的 `notify_keyspace_events`。Redis 在线协议上用的是连字符，而两者之间没有桥：这个参数在连接上既读不到也写不了。一个在连接时就设置它的库——这是正常做法——会在它的第一秒撞上 `ERR unknown parameter`，而引擎其实一直都有这个能力。

**要做什么。** 除非你曾经绕过它，否则什么都不用做。如果你为了迁就一个想自己设置它的客户端而把它写进了配置文件，现在可以把那处绕行删掉，让客户端自己去干。

```
CONFIG SET notify-keyspace-events Ex     → +OK
CONFIG GET notify-keyspace-events        → "Ex"
CONFIG SET notify-keyspace-events ZZZ    → -ERR CONFIG SET failed for
                                           'notify-keyspace-events': unknown flag char 'Z'
```

标志位就是 Redis 那一套：`K` keyspace、`E` keyevent、`g` 通用、`$` string、`l` list、`s` set、`h` hash、`z` zset、`t` stream、`x` expired、`e` evicted、`n` new-key，以及 `A` 作为 `g$lshzxet` 的别名（按 Redis 对 `A` 的约定，除 `n` 以外的全部类别）。标志在写入时校验，而不是存下来再默默忽略——所以打错字是在你打错的那一刻报错，而不是在你需要事件的那一刻沉默。

---

## 2. 五条 RESP3 回复现在带上了真实类型

**场景。** 你的客户端协商了 RESP3——`HELLO 3`——并且按回复类型解码。也就是开了 `protocol=3` 的 redis-py、v5 起默认如此的 node-redis，以及建立在它们之上的东西。**如果你的客户端说的是 RESP2（多数场景下仍是默认），这一节跟你无关。**

**原来错在哪。** 有五条命令在已经协商了 RESP3 的连接上，发的还是 RESP2 的形状：

| 命令 | 原来发的 | 现在发的（与 Redis 8.10.1 一致） |
|---|---|---|
| `ZPOPMIN` | 分数是 bulk string | 分数是 **double** |
| `ZADD … INCR` | 新分数是 bulk string | 新分数是 **double** |
| `GEOPOS` | 坐标是 bulk string | 坐标是 **double** |
| `SPOP key count` | 一个 **array** | 一个 **set** |
| `HRANDFIELD … WITHVALUES` | 平铺的列表 | **成对嵌套** |

于是按类型解码的客户端在该拿到数字的地方拿到了字符串，在该拿到集合的地方拿到了列表。多数客户端会做隐式转换，所以它表现为数据里悄悄错掉的类型、而不是一个报错——这也是它藏了这么久的原因。

**要做什么。** 如果你自己在做这层转换（比如 RESP3 的 `ZPOPMIN` 之后 `float(score)`），那层转换现在是多余的，但无害。如果你有 golden file / 快照测试录下了 RESP3 连接上的 RESP2 形状，重录一次：那条测试钉住的是一个缺陷。

**这是怎么发现的**，因为这件事本身说明了什么可信：`bench/resp3gate.sh` 去问被钉住的那版 Redis——`HELLO 3` 之下哪些动词会换形状——然后要求 kevy 在完全相同的位置跟着动。它不是一份"我们认为哪些命令是 RESP3-aware"的手写清单，清单每一轮都从对手那里来，而且找到的数目少得不合理时门禁会拒绝通过。有 11 个动词会换形状；kevy 现在一个都不相左。

---

## 3. `HRANDFIELD` 实现了

**场景。** 你要从一个 hash 里抽取字段——功能开关、A/B 分桶、分片任务队列、"给我看几个"这类接口——而以前只能 `HGETALL` 把整个 hash 拉回来，在自己代码里挑。

```
HRANDFIELD key                      → 一个字段
HRANDFIELD key 5                    → 至多 5 个「不重复」字段（hash 更小时就更少）
HRANDFIELD key -5                   → 恰好 5 个，「允许重复」
HRANDFIELD key 5 WITHVALUES         → 每个字段连同它的值
```

**count 的正负号就是整个 API。** 正数是**子集**：字段互不重复，hash 比你要的小就给得更少。负数是**抽样**：恰好 `|count|` 条，同一个字段可能出现两次。这是 Redis 的约定，kevy 完全照办，包括键不存在和 count 为 0 时都返回空数组。

**它省的是什么。** 对一个大 hash 用 `HGETALL`，是把整个 hash 放上线，好让你从里面挑三个字段；`HRANDFIELD key 3` 放上线的就是三个。一个几千字段的 hash、每个请求抽一次，这就是每次调用一千字节和几十字节的差别——占大头的是那条回复；选取本身是在字段列表上做部分 Fisher-Yates，只打乱会被返回的那一段前缀。

kevy 的四种 hash 表示（包括打包行）它都能处理，所以你不必知道某个键此刻用的是哪一种。

---

## 4. `GEOPOS` 遇上类型不对的键

**这是唯一变了的一条回复，也是现有代码唯一可能察觉到的地方。**

原来，对一个存着字符串的键问 `GEOPOS`，回来的是一个数组头**然后**才是一个错误——`*1\r\n-WRONGTYPE …`——于是读这条回复的客户端看到的是"一个首元素是错误的数组"。数组头在类型被解析出来之前就已经发出去了。

现在它回 `-WRONGTYPE Operation against a key holding the wrong kind of value`，除此之外什么都没有，跟 Redis 一模一样。

**谁会察觉。** 那些检查 `reply[0]` 里有没有错误标记、而不是检查这条回复**本身是不是**一个错误的代码。这类代码现在会在错误该在的位置看到错误。多数客户端库本来就把旧形状当成畸形回复处理，所以实际效果是把错误处理修好，而不是弄坏。

另外三种情况没变：成员存在就给坐标，成员不存在和键不存在都给空数组（null array）。

---

## 什么原样带过去

- **线协议，除了 §2 和 §4。** 其余每一条 RESP2 / RESP3 回复都与 6.2.2 逐字节相同。
- **数据目录。** AOF、快照、value log 与每一种 checkpoint 都跟以前一样打开，两个方向都行。没有迁移步骤，也没有单向门。
- **复制。** 复制流没有变：`HRANDFIELD` 是读命令，永远不会进流，而本次发布也没有碰复制路径的任何东西。6.2.x 副本与 6.3.0 主库两个方向都能配对，所以可以一个节点一个节点地升。
- **所有 crate 与绑定的 API。** 从 6.2.2 到 6.3.0 没有任何代码改动。

---

## 6.3.0 还改了什么——量测，不是行为

下面这些不影响任何行为，但它们决定了公布出来的数字值多少钱。

- **每一个基准对手都钉到了确切版本，并对着各自上游的 latest stable 核过。** Redis 8.10.1、valkey 9.1.2、Dragonfly 1.40.2、postgres 18.6，外加一致性套件驱动的四个客户端库（go-redis 9.22.0、StackExchange.Redis 3.1.31、node-redis 6.2.1、redis-py 8.1.0）。这四个里有两个自 2024 年起就冻着，另两个根本没写版本。现在测量装置会问每个引擎自己是什么版本，对不上就拒绝出数——所以一张公布的表能把对手精确到修订号。抬高一个钉子是一套写下来的流程，而不是一次编辑：见 `.claude/skills/competitor-anchors/SKILL.md`。
- **被钉住的那版 Redis 提供的每一条命令，现在都有交代。** 它的 599 条命令与子命令里，kevy 实现了 206 个动词；剩下的当中，256 条豁免且每条都写了理由，80 条由具名 RFC 认领。没有一条是未分类的，而且这个计数挂在棘轮上、只能往下走——所以将来某个 Redis 版本新增命令时，它会以"一个待做的决定"落地，而不是以沉默落地。如果你一直在想某个你需要的动词是不是缺了，答案现在写在 `bench/COMMAND-COVERAGE.json` 里，不用靠试出来。

6.3.0 的实测数字——lx64、三整轮、逐格取中位数、读各引擎自己的命令计数器、`-c 50 -P 16`：

| 动词 | kevy | Redis 8.10.1 | 对比 Redis |
|---|---:|---:|---:|
| GET | 7,489,119/s | 5,631,398/s | 1.33x |
| SET | 6,824,662/s | 2,567,607/s | 2.66x |
| INCR | 6,753,558/s | 3,294,927/s | 2.05x |
| SADD | 6,152,617/s | 3,753,131/s | 1.64x |
| HSET | 4,002,580/s | 2,966,288/s | 1.35x |
| ZADD | 3,242,967/s | 2,818,626/s | 1.15x |
| LPUSH | 3,142,699/s | 2,860,306/s | 1.10x |

七格里 kevy 最差的一轮都赢过每个对手最好的一轮；最窄的是 LPUSH 与 ZADD，对 Redis 1.08x。本次发布没有改动服务路径，所以这是把 6.2.2 的数字重测了一遍、而不是变快了——完整条目（含 valkey、Dragonfly 与轮间离散度）在 `bench/PERF-LEDGER.md`。

---

## 怎么拿到

```toml
# Cargo.toml
kevy-embedded = "6.3.0"
```

```sh
npm install @goliapkg/kevy@6.3.0          # wasm
npm install @goliapkg/kevy-node@6.3.0     # Node 原生
npm install @goliapkg/kevy-bin@6.3.0      # 服务器二进制
pip install kevy==6.3.0
go get github.com/goliajp/kevy-go/v6@v6.3.0
```

用 `ghcr.io/goliajp/kevy:latest` 的容器用户，下次拉取就是 6.3.0。如果你钉了 tag，它是 `ghcr.io/goliajp/kevy:6.3.0`。

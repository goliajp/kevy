# Lua 脚本

kevy 的服务端 Lua 脚本：怎么用 `EVAL` / `EVALSHA` 跑原子脚本、有哪些绑定，以及如何选用比 5.1 更新的 Lua 方言。

## 什么时候需要它

以下情况该考虑 Lua 脚本：

- 想对一个键原子地执行一小段多命令序列（先查后写、条件计数器、分布式锁）。
- 想把单条命令表达不了的读-改-写逻辑推进服务器，省掉往返。
- 想原样跑生态库发布的脚本（BullMQ、Sidekiq、Bee Queue、Redlock、滑动窗口限流器）。

如果只是普通的单命令访问，或者要在很多键上做带显式乐观锁的事务，请用普通命令或 `MULTI` / `EXEC`。

## 核心思路

`EVAL` 把一段 Lua 源码送到服务器，编译，然后在持有 `KEYS[1]` 的那个 shard 上执行。脚本运行期间，该 shard 上没有任何其他命令会与它交错——整个脚本是一个原子单元。脚本内部，`redis.call("CMD", ...)` 顺着正常命令路径分派回去，`KEYS` 和 `ARGV` 给出 1 起始下标、二进制安全的输入访问，脚本的返回值被编排成 RESP。加载过的脚本按 SHA1 缓存，所以 `SCRIPT LOAD` + `EVALSHA` 让客户端每次只发哈希、不发脚本体。Lua 运行时是 [luna](https://github.com/goliajp/luna)，一个纯 Rust 的 5.1 – 5.5 解释器。默认方言是 Lua 5.1（Redis 生态里每一段脚本都是照这个写的）；一行 shebang 就能让单个脚本选用 5.2、5.3、5.4 或 5.5。

## 实例：带上限的计数器

这段脚本把计数器加 1，但仅当加完之后仍不超过上限。返回新值，超限则返回 `nil`。

```lua
-- KEYS[1] = counter key
-- ARGV[1] = cap (integer)
local cur = tonumber(redis.call("GET", KEYS[1]) or "0")
local cap = tonumber(ARGV[1])
if cur + 1 > cap then
  return nil
end
return redis.call("INCR", KEYS[1])
```

### 内联：`EVAL`

```sh
redis-cli -p 6004 EVAL \
  "local cur = tonumber(redis.call('GET', KEYS[1]) or '0')
   local cap = tonumber(ARGV[1])
   if cur + 1 > cap then return nil end
   return redis.call('INCR', KEYS[1])" \
  1 quota:user:42 5
# (integer) 1
# … four more calls …
# (integer) 5
# next call:
# (nil)
```

### 缓存：`SCRIPT LOAD` + `EVALSHA`

```sh
SHA=$(redis-cli -p 6004 SCRIPT LOAD \
  "local cur = tonumber(redis.call('GET', KEYS[1]) or '0')
   local cap = tonumber(ARGV[1])
   if cur + 1 > cap then return nil end
   return redis.call('INCR', KEYS[1])")
echo "$SHA"
# e.g. 7c3e0a9b1d4f...

redis-cli -p 6004 EVALSHA "$SHA" 1 quota:user:42 5
# (integer) 1

redis-cli -p 6004 SCRIPT EXISTS "$SHA"
# 1) (integer) 1
```

客户端拿一个服务器从没见过的哈希去发 `EVALSHA` 时（冷启动、`SCRIPT FLUSH` 之后），kevy 返回 `-NOSCRIPT`，客户端应当回退到 `EVAL`。按 SHA1 缓存的脚本体**包含** shebang 那一行，所以同一份源码的 5.1 版和 5.4 版是两个不同的缓存条目。

### 用 shebang 选 Lua 5.4 方言

```lua
#!lua version=5.4
-- ARGV[1] = max retries before giving up
local tries = tonumber(ARGV[1])
local i = 0
::again::
i = i + 1
local ok = redis.call("SET", KEYS[1], "owned", "NX", "PX", 3000)
if type(ok) == "table" and ok.ok == "OK" then
  return i
end
if i < tries then goto again end
return redis.error_reply("LOCK_FAILED")
```

`goto` / 标签和整数类型的算术是 5.3+ 的特性；这行 shebang 把这一个脚本路由到 5.4 的 VM 池，其他脚本照旧跑在 5.1 上。

## 绑定

| 符号 | 行为 |
|---|---|
| `redis.call(cmd, ...)` | 分派一条 kevy 命令。RESP 错误会抛出 Lua error 并中止脚本，除非用 `pcall` 接住。 |
| `redis.pcall(cmd, ...)` | 同样的分派，但 RESP 错误以 `{err = "msg"}` 返回，而不抛出。 |
| `redis.error_reply(msg)` | 构造 `{err = msg}`；从脚本返回时编排成 `-msg\r\n`。 |
| `redis.status_reply(msg)` | 构造 `{ok = msg}`；编排成 `+msg\r\n`（simple string）。 |
| `redis.sha1hex(s)` | 输入字节的 40 字符小写十六进制 SHA-1。 |
| `redis.replicate_commands()` | 空操作。每个脚本本来就作为一个整体原子复制。 |
| `KEYS` | `EVAL` 调用里声明的 `numkeys` 个字节串组成的表，下标从 1 起。二进制安全。 |
| `ARGV` | 其余参数组成的表，下标从 1 起。二进制安全。 |
| `cjson.encode(v)` / `cjson.decode(s)` | 纯 Rust JSON 编解码器。接口面与 Redis 的 `cjson` 库一致。 |
| `cmsgpack.pack(v)` / `cmsgpack.unpack(s)` | 纯 Rust MessagePack 编解码器。接口面与 Redis 的 `cmsgpack` 库一致。 |

Lua 返回值按标准 Redis 规则编排成 RESP：`nil` 和 `false` 变 nil-bulk，`true` 变 `:1`，整数以及无损整数浮点变整数回复，其他浮点和字符串变 bulk string，`{ok=...}` 变 simple string，`{err=...}` 变错误回复，普通数组变 multi-bulk 回复（遵守首个 nil 截断规则）。

## 方言选择

| 脚本首行 | 使用的方言 |
|---|---|
| （无 shebang） | Lua 5.1（默认；BullMQ、Sidekiq、Redlock 以及绝大多数已发布的 Redis-Lua 片段都假定它） |
| `#!lua version=5.1`（或 `51`） | Lua 5.1，显式指定 |
| `#!lua version=5.2`（或 `52`） | Lua 5.2——`goto`、`_ENV`、ephemeron table |
| `#!lua version=5.3`（或 `53`） | Lua 5.3——整数子类型、位运算符、`//`、`string.pack` / `string.unpack` |
| `#!lua version=5.4`（或 `54`） | Lua 5.4——to-be-closed 变量、整数 `for` 语义、新的位运算角落 |
| `#!lua version=5.5`（或 `55`） | Lua 5.5——已发布的最新方言 |
| `#!lua version=<其他>` | `-ERR unknown lua version: <X>` |

shebang 行上的 Redis 7.0 Functions 元数据（`flags=`、`name=`）会被解析并忽略，所以为 Functions 接口面写的脚本可以干净地在 `EVAL` 下加载。

想要纯 Redis 生态兼容性的部署，可以在配置里把可接受的方言钉死：`[lua] allow_dialects = ["5.1"]` 会拒绝任何 shebang 要求更新 VM 的脚本。默认（空列表）接受全部五种。

## 取舍与限制

- **没有文件系统、网络或 OS 访问。** `io`、`os`、`package`、`debug`、`coroutine` 都不加载。脚本无法打开文件、建立 socket、派生进程，或读取环境变量。
- **不加载字节码。** `load(bytecode)` 和 `string.dump` 被封死。只有 Lua 源码能进 VM，这堵死了历史上屡次攻破 Lua 沙箱的“字节码验证器”逃逸路线。
- **白名单标准库。** 可用的是 `base`、`math`、`string`、`table`、`cjson`、`cmsgpack`。其他标准模块一概不在。
- **逐脚本的时间预算。** 每次 `EVAL` 都在一份由 `[lua] time_limit_ms` 推导出的指令预算下运行（默认 5000 ms ≈ 2 亿条指令——与 Redis 默认的 `lua-time-limit` 同一条上限；设 `0` 关闭）。超出返回一个可被接住的 Lua error 并中止脚本。
- **逐脚本的内存预算。** 每个脚本跑在一份从逐方言 VM 池里取出的全新解释器状态上；调用期间创建的表和字符串在返回时回收。调用之间没有共享的可变 Lua 状态——要持久化什么，用 kevy 的键。
- **不能嵌套 `EVAL`。** 脚本里调用 `redis.call("EVAL", ...)` 返回错误，与 Redis 行为一致。
- **一个脚本只能在一个 shard 上。** 所有 `KEYS` 必须哈希到同一个 slot。触及跨 shard 多个键的脚本在分派时拿到 `-CROSSSLOT`。
- **JIT 是设计上关闭的。** 直接用解释器；luna 里的 Cranelift JIT 没有链进 kevy 服务器，这既让依赖面保持最小，也避开了 JIT 编译期的停顿。

## FAQ

**我现有的 Redis Lua 脚本能不改就跑吗？**
如果它面向 Lua 5.1（Redis 的默认），并且只用到 `redis.call` / `redis.pcall` / `redis.error_reply` / `redis.status_reply` / `KEYS` / `ARGV` / `cjson` / `cmsgpack`，能。如果它依赖 debug 库、OS 库，或者加载预编译字节码，不能——那些都被沙箱挡在外面了。

**怎么跨不同 shard 上的键跑一个脚本？**
跑不了——原子性正是它存在的全部意义。要么把工作拆成逐 shard 的脚本、在客户端侧协调，要么设计键布局让相关的键共享同一个 hash tag（`{user:42}:quota` 和 `{user:42}:counter` 路由到同一个 shard）。

**`EVALSHA` 返回 `-NOSCRIPT`，怎么办？**
把同一份脚本体用 `EVAL` 重发一次。服务器会重新缓存它并回答这次调用。大多数客户端库会自动处理这个回退。`SCRIPT FLUSH` 和进程重启都会清空缓存。

**脚本里能调 `BLPOP` 之类的阻塞命令吗？**
不能。`EVAL` 内部的阻塞命令会击穿原子性契约——那个 shard 要么冻在原地等自己，要么就得交错去干别的活。从脚本里分派阻塞命令会返回错误。

**已经有更新的版本了，为什么默认还是 Lua 5.1？**
Redis 生态里发布的每一段脚本都假定 5.1。默认 5.1 意味着那些脚本可以复制粘贴直接跑，不会在整数子类型、位运算符或 `goto` 上冒出意外。真的想要新特性时，用 shebang 把单个脚本切到新方言，是一行的改动。

# 错误回复目录

kevy 在线协议层发出的每一种错误回复的速查表：什么触发它、客户端下一步该做什么。

## 怎么读这张表

kevy 的错误以 RESP simple-error 字符串传输：`-<PREFIX> <message>\r\n`。第一个以空白分隔的 token 是**前缀**（`ERR`、`WRONGTYPE`、`MOVED`、`CROSSSLOT`……）。客户端库通常把它暴露成错误变体或异常，其 `.kind` / `.code` / 类名与前缀对应；行的其余部分是给人读的消息。

本目录按前缀分组。如果你按结构处理错误（推荐），就匹配前缀；后面的消息是给日志和运维看的，不是给解析器看的。

错误是 kevy 面向用户契约的一部分。新增、改名或挪用一个前缀，对做模式匹配的客户端来说都是破坏性变更。

## 核心目录

| 前缀 | 触发时机 | 恢复 / 下一步 |
|--------|---------------|----------------------|
| `-ERR unknown command '<cmd>'` | 命令名未实现或不识别。 | 查 README 命令覆盖表；确认拼写。 |
| `-ERR wrong number of arguments for '<cmd>' command` | argc 与命令接受的形状不符。 | 按文档的参数个数重发。 |
| `-ERR value is not an integer or out of range` | INCR/DECR 类命令作用在无法解析为 i64 的值上，或数值参数越界。 | 用 `SET` 覆盖成可解析的整数；夹紧输入。 |
| `-ERR no such key` | RENAME / RENAMENX / COPY / GETEX 的目标 key 不存在。 | 按 key 不存在处理（必要时先用 `EXISTS` 预检）。 |
| `-ERR kevy only supports DB 0` | `SELECT N` 且 N ≠ 0。 | kevy 没有多 DB；用独立实例或 key 命名空间。 |
| `-ERR MULTI calls can not be nested` | 已在 MULTI 块内又发 `MULTI`。 | 等 `EXEC` / `DISCARD` 之后再开新事务。 |
| `-ERR EXEC without MULTI` | 没有打开的事务却发了 `EXEC`。 | 与 `MULTI` 配对，或丢弃该命令。 |
| `-ERR DISCARD without MULTI` | 没有打开的事务却发了 `DISCARD`。 | 与 `MULTI` 配对，或丢弃该命令。 |
| `-ERR WATCH inside MULTI is not allowed` | 在 MULTI 块内发 `WATCH`。 | 在 `MULTI` 之前发 `WATCH`。 |
| `-ERR <cmd> not allowed inside MULTI` | 不能排队的命令（pub/sub、`WATCH`、`HELLO`、`RENAME`）被排进了 MULTI。 | 把这些命令放在事务之外。 |
| `-ERR Protocol error` | 入站字节不是合法 RESP。 | 重连重试；若持续出现，审计客户端序列化器。 |
| `-ERR CONFIG SET failed for '<key>': <reason>` | CONFIG 字段未知或取值越界。 | `CONFIG GET *` 查看支持的字段与当前值。 |
| `-ERR CONFIG REWRITE could not write <path>: <io-error>` | 配置 TOML 路径缺失或不可写。 | 检查 `--config` 路径与文件系统权限。 |
| `-WRONGTYPE Operation against a key holding the wrong kind of value` | 命令作用在已存在但类型不同的 key 上。 | 见 [Wrong-type 规则](#wrong-type-规则)。 |
| `-EXECABORT Transaction discarded because of previous errors.` | MULTI 期间某条排队命令是未知动词或参数过少，EXEC 拒绝整批、什么都不执行。 | 修正出错的排队命令，重新 `MULTI` / 排队 / `EXEC`。 |
| `-MOVED <slot> <host:port>` | key 的哈希槽不属于本节点。 | 见[集群路由回复](#集群路由回复)。 |
| `-CROSSSLOT Keys in request don't hash to the same slot` | 多 key 命令跨了多个哈希槽。 | 见[集群路由回复](#集群路由回复)。 |
| `-MISDIRECTED writer is <host:port>` | 写入落在了不拥有该 key scope 的节点上，或 `REPL.WAIT` 在此副本上无法提供读己之写（超时或 generation 不匹配）。 | 见[集群路由回复](#集群路由回复)；对 `REPL.WAIT`，去 `<host:port>` 读主节点。 |
| `-QUIESCED migrating to <host:port>` | 槽或 scope 正在迁移、在本节点冻结，或节点正处于向 `<host:port>` 的 `FAILOVER` 交接中。 | 见[集群路由回复](#集群路由回复)。 |
| `-OOM command not allowed when used memory > 'maxmemory'` | 超限后在 `noeviction` 策略下发写类命令。 | 调高 `maxmemory`、设置逐出策略（如 `allkeys-lru`），或 `DEL` 腾出空间。已有数据完好。 |
| `-READONLY You can't write against a read only replica.` | 向副本节点发送写命令。 | 发给主节点，或使用路由客户端。 |
| `-NOREPLICAS Not enough good replicas to write.` | `min_replicas_to_write = N` 的主节点看到的健康副本少于 N。 | 退避重试；恢复副本（见 [docs/availability.md](availability.md)）。 |
| `-NOREPLICAS primary lost quorum; writes fenced` | elect 多数派主节点联系不上严格多数的同僚（分区少数侧）；写入在租约窗口内自我围栏。 | 退避重试——分区愈合后围栏解除，或多数派选出新主节点、路由客户端会找到它。 |
| `-STALE replica is stale; read the primary or raise replica_max_staleness_ms` | 读请求发给了上一次主节点心跳早于其 `replica_max_staleness_ms` 界限的副本。 | 在副本追平前读主节点，或调高/关闭该界限。 |
| `-READONLY can't write against a read-only script` | 通过 `EVAL_RO` / `EVALSHA_RO` 求值的脚本尝试写入。 | 改用可写的 `EVAL` / `EVALSHA` 变体。 |
| `-NOSCRIPT No matching script. Please use EVAL.` | `EVALSHA <sha>` 请求的脚本不在缓存中。 | 直接调 `EVAL`（kevy 自动缓存）或先 `SCRIPT LOAD`。 |
| `-LOADING kevy is loading the dataset in memory` | 副本正在从主节点接收 full-resync 快照期间收到读请求。 | 等待重试（窗口以快照传送时长为界）；loading 期间 `PING`、`INFO`、`HELLO` 照常应答，健康检查与监控不中断。 |
| `-ERR No such client address in the list` | 旧式 `CLIENT KILL <addr:port>` 没有匹配到任何连接。 | 用 `CLIENT LIST` 列出在线连接后按现存 `addr` 重发；过滤式形态（`CLIENT KILL ID\|ADDR\|LADDR …`）返回计数 0 而不报错。 |

### kevy 从不发出的前缀

kevy 有意不在协议层做认证与授权；下面这些 Redis 兼容前缀刻意缺席：

- `-NOAUTH`——没有 `AUTH` 命令面。
- `-WRONGPASS`——没有密码检查。
- `-NOPERM`——没有 ACL 系统。
- `-MISCONF`——持久化失败（AOF 追加或后台保存）会记到 stderr，节点继续用一致的内存状态服务；kevy 不把耐久性错误变成客户端可见的应答。重启时重放已落盘的部分。
- `-BUSY`——kevy 没有交互式 `SCRIPT KILL`。脚本改由指令预算封顶（`[lua] time_limit_ms`，`0` = 不限）；超预算的脚本就地中断，其 `EVAL` 返回普通的 `-ERR` Lua 错误，失控脚本不会卡住 shard 等别的客户端来打断。

如果你的客户端认为这些前缀可能出现，在 kevy 上请按不可达处理。访问控制交给部署边界（kevy 默认绑定 `127.0.0.1`）。

## Wrong-type 规则

`WRONGTYPE` 回复表示这个 key 已存在，但 Redis 数据类型与命令期望的不同。规则如下：

- 类型在 key 创建时确定，直到 key 被删除（或过期）前保持不变。
- `DEL <key>` 之后重发原命令会成功（类型已被重置）。
- `EXPIRE` / `PERSIST` / `TYPE` / `OBJECT ENCODING` / `EXISTS` / `DEL` / `UNLINK` 与类型无关，永不抛 `WRONGTYPE`。
- key 不存在时永远不会返回 `WRONGTYPE`；缺失 key 的语义遵循各命令的文档行为（`GET` 返回 nil、`LPUSH` 创建列表，依此类推）。

恢复路径永远是二选一：换一个 key，或 `DEL` 现有 key 后按预期类型重建。

## 集群路由回复

这些前缀只在 kevy 以路由模式（cluster 或 scoped）运行时出现。非集群客户端可能永远见不到它们。

- **`-MOVED <slot> <host:port>`**——key 的哈希槽永久归属另一节点时触发。客户端应重连 `<host:port>` 并重发。集群感知客户端（如 `redis-cli -c`、cluster 模式的 `ioredis`）会透明跟随重定向并更新槽映射。
- **`-CROSSSLOT Keys in request don't hash to the same slot`**——多 key 命令（`MGET`、`MSET`、多 key 的 `DEL`、多 `KEYS` 的 `EVAL`、`SUNIONSTORE` 等）的 key 没有全部落在同一槽时触发。用共享的 `{hashtag}` 段把 key 放到一起，或在客户端按槽拆分命令。
- **`-MISDIRECTED writer is <host:port>`**——写入落在不拥有该 key scope 的节点上时触发。路由客户端跟随重定向；手动客户端应重连 `<host:port>`。
- **`-QUIESCED migrating to <host:port>`**——槽或 scope 正在迁移、在本节点冻结期间触发。客户端应视同 `MOVED`，去 `<host:port>` 重试。迁移完成后，那个节点会给出权威应答。
- 完整路由模型见 [docs/](https://github.com/goliajp/kevy/tree/develop/docs) 下的协议说明。

## FAQ

**我的客户端把 `-MOVED` 当致命错误——怎么办？**
这个客户端不是集群感知的。要么换集群感知客户端（如 `redis-cli -c`、带 `Cluster` 的 `ioredis`、带 `RedisCluster` 的 `redis-py`、kevy 路由客户端），要么在驱动外包一层：捕获 `MOVED`、重连消息里的主机、重发命令。

**单 key 命令收到了 `-CROSSSLOT`——是 bug 吗？**
不是。`CROSSSLOT` 只对多 key 命令触发。如果它出现在看似单 key 的调用上，说明这条命令实际是多 key 的（比如带两个 `KEYS` 的 `EVAL`、带源和目的的 `SUNIONSTORE`）。用 `{tag}` 记法强制同槽放置，或拆分调用。

**收到了 `-OOM`——我的数据坏了吗？**
没有。`-OOM` 在命令准入时就被拒绝；这次写入根本没有落地。keyspace 与命令之前的状态完全一致。腾出空间（`DEL` / 设置逐出策略 / 调高 `maxmemory`）后重试。

**`-LOADING` 一直出现——该等多久？**
等 full-resync 快照传送完成（时长与数据集大小和链路速度成正比）。loading 期间 `PING`、`INFO`、`HELLO` 照常应答（`INFO replication` 报告 `loading:1`），健康检查不受影响。副本只有在重连时落后主节点 backlog 太多才会进入这个状态；如果 `-LOADING` 反复出现，调高 `[replication] replication_buffer_size` 或排查链路。

**排队的 MULTI 命令返回了 `-EXECABORT`——有写入被应用吗？**
没有。`EXECABORT` 表示事务作为一个整批被拒绝；排队序列里什么都没执行。修正出错的命令后重新 `MULTI`。

## 扩展面（IDX. / VIEW. / FEED.）

扩展 verb 遵循同一套前缀契约。每个错误都自我解释：点名 verb 和对象，并指向发现面（撞上错误的 agent 可以在带内恢复）。

| 前缀 | 触发时机 | 恢复 |
|---|---|---|
| `ERR <VERB> '<name>': bad arguments — run COMMAND DOCS <VERB> for the syntax` | IDX./VIEW. verb 参数解析失败 | `COMMAND DOCS <verb>` 返回完整语法串 |
| `ERR no such index '<name>' (IDX.LIST enumerates them)` | 查询点名了不存在的索引 | `IDX.LIST` / `VIEW.LIST` 枚举目录 |
| `INDEXBUILDING index '<name>' is still building (poll IDX.LIST until state=ready)` | 查询撞上了建索引后的回填窗口 | 轮询 `IDX.LIST` 的 `state`；见 [docs/migration.md](migration.md) |
| `INDEXOVERBUDGET index '<name>' build exceeded MAXMEM (raise maxmemory or DROP the index)` | 构建触到内存预算 | 调高 `maxmemory` 或 `IDX.DROP` |
| `FEEDRESYNC <gen> <tail>` | FEED 游标不再可服务（generation 递增或超出 backlog） | 从新快照 + 返回的游标重启消费；见 [docs/cdc.md](../cdc.md) |

## 更新这份目录

如果你新增或修改了发出 `-<PREFIX> ...` 回复的代码路径：

1. 更新[核心目录](#核心目录)里的行（或加一行）。
2. 引入新前缀时，扩展线协议层混沌测试 [crates/kevy/tests/wire_torture_chaos.rs](https://github.com/goliajp/kevy/blob/develop/crates/kevy/tests/wire_torture_chaos.rs)。
3. 在发布它的版本的 [CHANGELOG.md](https://github.com/goliajp/kevy/blob/develop/CHANGELOG.md) 里记一笔。

错误是客户端契约的一部分。悄悄改消息文本，会弄坏生态里对其做模式匹配的库。

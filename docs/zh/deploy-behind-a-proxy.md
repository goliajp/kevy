# 把 kevy 放在代理后面部署

kevy 没有 AUTH、没有 TLS，而且不会有——这是章程里的决定，不是等着被补上的缺口。**所有认证与加密都发生在进程前面。** 这一章就是那个配方，写出来是给人照抄的，不是给人改编的。

## kevy 会开什么

| | 端口 | 说明 |
|---|---|---|
| 默认 | **一个**（`6004`） | `--threads N` 是在**同一个**端口上开 N 个 `SO_REUSEPORT` 监听，不是 N 个端口 |
| `--cluster` | **1 + N** | 主端口，外加每分片一个 `port+1+i` |
| `KEVY_UNIX_SOCKET=<path>` | 不变 | unix socket 是**加上去的**；TCP 监听照旧在 |

默认绑定是 `127.0.0.1`。这本来就是这一章想要的形状：引擎只在本机可达的地方监听，唯一带公网地址的东西是那个终止器。

## 形状

```
   client ──TLS──▶  terminator  ──plain──▶  kevy
  (rediss://)     (stunnel / HAProxy /     127.0.0.1:6004
                   nginx stream)           或 /run/kevy/kevy.sock
```

kevy 这边什么都不用改。RESP 里没有主机名、没有 SNI、没有绝对 URL——前面放一个字节代理，对两侧都是不可见的。

## RESP 不是 HTTP

HTTP 反向代理载不动 RESP，**stock Caddy 也不行**：它的核心没有 layer-4 模块，所以无论 Caddyfile 怎么写，光靠 `caddy` 都没法给 kevy 挡上 TLS。你需要一个 TCP 层的终止器：

- **stunnel**——最小，哪里都打过包，只干这一件事；
- **HAProxy** 的 `mode tcp`——如果你本来就在跑 HAProxy，或者想把健康检查与故障转移放在同一处；
- **nginx** 的 `stream` 模块——同理，如果 nginx 本来就在。

### stunnel → 回环端口

```ini
[kevy]
accept  = 0.0.0.0:6379
connect = 127.0.0.1:6004
cert    = /etc/kevy/tls/fullchain.pem
key     = /etc/kevy/tls/privkey.pem
```

### HAProxy → unix socket

更紧一档：引擎的 socket 文件带文件系统权限，于是即使在本机，也只有终止器这一个进程能碰到它。

```
listen kevy
    bind :6379 ssl crt /etc/kevy/tls/kevy.pem
    mode tcp
    timeout client 0
    timeout server 0
    server kevy unix@/run/kevy/kevy.sock
```

带 socket 启动 kevy：

```console
KEVY_UNIX_SOCKET=/run/kevy/kevy.sock kevy --dir /var/lib/kevy
```

关于那个路径有两件事。**路径已存在时 kevy 拒绝启动**——它不会去覆盖一个不是自己创建的路径——所以重启时清理它，或者用每次一变的路径。以及 `127.0.0.1` 上的 TCP 监听照旧在；socket 是增补，不是替代。

### nginx stream → unix socket

```nginx
stream {
    upstream kevy { server unix:/run/kevy/kevy.sock; }
    server {
        listen 6379 ssl;
        ssl_certificate     /etc/kevy/tls/fullchain.pem;
        ssl_certificate_key /etc/kevy/tls/privkey.pem;
        proxy_pass kevy;
        proxy_timeout 1h;
    }
}
```

注意三份配置里的超时：一个阻塞的 `BLPOP`、或者一个闲着的 Pub/Sub 订阅者，会把连接一直开着而上面一个字节都不走，想开多久开多久。一个会回收空闲连接的代理，看起来会**跟 kevy 在丢订阅者一模一样**。

## 客户端这一侧

任何开了 TLS 的标准 Redis 客户端——`rediss://host:6379`——都能原封不动地跟被终止过的 kevy 说话。

**`kevy-cli` 不行。** 它对 `rediss://` 直接回 `Unsupported`，因为 kevy 出厂就没有 TLS，CLI 也没有 TLS 栈可以借它。这是一条值得提前安排、而不是事到临头才发现的运维后果：要么在主机上直接管，要么走 SSH 隧道：

```console
ssh -N -L 6004:127.0.0.1:6004 you@host   # 然后：kevy-cli -p 6004
```

## 只暴露必要的东西

用默认绑定时，**kevy 一条防火墙规则都不需要**——它本来就不在主机之外可达。要开的端口只有终止器那一个。如果你非要把 kevy 绑到一个真实网卡上，那么防火墙就在做原本由回环绑定免费做掉的那件事，而且它将是网络与一个**没有认证的数据库**之间唯一的东西。

## 集群模式撑不过这一层

单机集群模式是给**同一台主机上的客户端**用的，代理不会把它延伸出去。`CLUSTER SLOTS` 与 `CLUSTER NODES` 广播的是 kevy 绑定的那个地址，并在 `0.0.0.0` 通配时替换成 `127.0.0.1`（广播一个不可路由的地址会让所有客户端搁浅）。**没有 announce-address 这个旋钮**，所以一个被告知 `127.0.0.1:6101` 的键感知客户端，从别处是跟不过去的。

如果你需要远端的键感知路由，那就绑一个可路由的地址，并把那 `1 + N` 个端口一一映射过去——那恰恰是"只暴露一个端口"的反面。二选一，它们不能同时成立。

## 哪些是实测的，哪些不是

写这一章时在当前这棵树上实测：

- 上表里的端口面，包括 `--cluster` 每分片开一个 `port+1+i`；
- 前面放一个普通 TCP 代理对 kevy 是透明的（`PING`、`SET`、`GET` 都穿过转发器）；
- **TLS 终止之后，RESP 对一个未经修改的 kevy 完整往返**——回环端口与 unix socket 两种都测了，TLS 1.3，`+PONG` / `+OK` / 存进去的值原样回来；
- stock Caddy（2.11.4）不带 layer-4 模块；
- `0.0.0.0` 绑定下 `CLUSTER SLOTS` 广播 `127.0.0.1`；
- `kevy-cli` 拒绝 `rediss://`。

那三段配置是各自产品干这件事的标准写法，**没有在这里跑过**。实测过的是它们所实现的那个形状。

## 参见

- [uds.md](uds.md)——unix socket 细节
- [cluster.md](cluster.md)——单机集群模式是给什么用的
- [tuning.md](tuning.md)——`--threads`，以及为什么少反而更快

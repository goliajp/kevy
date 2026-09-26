# 把 kevy 放在代理后面部署

kevy 没有 AUTH，也没有 TLS。**认证和加密都在进程前面做。** 这一章就是做法，照抄即可，不需要自己改编。

## kevy 会开什么

| | 端口 | 说明 |
|---|---|---|
| 默认 | **一个**（`6004`） | `--threads N` 是在**同一个**端口上开 N 个 `SO_REUSEPORT` 监听，不是 N 个端口 |
| `--cluster` | **1 + N** | 主端口，外加每个 shard 一个 `port+1+i` |
| `KEVY_UNIX_SOCKET=<path>` | 不变 | unix socket 是**额外加的**，TCP 监听照旧在 |

默认绑定是 `127.0.0.1`，这正是本章要的形状：引擎只在本机能访问的地方监听，唯一有公网地址的是前面的终止器。

## 形状

```
   client ──TLS──▶  terminator  ──plain──▶  kevy
  (rediss://)     (stunnel / HAProxy /     127.0.0.1:6004
   + 客户端证书      nginx stream)           或 /run/kevy/kevy.sock
```

kevy 这边什么都不用改。RESP 里没有主机名、没有 SNI、也没有绝对 URL，前面放一个字节代理，两边都感觉不到。

## RESP 不是 HTTP

HTTP 反向代理转发不了 RESP，**原版 Caddy 也不行**：它的核心没有四层模块，Caddyfile 怎么写都没法单靠 `caddy` 给 kevy 加上 TLS。需要一个 TCP 层的终止器：

- **stunnel**：最小，各发行版都有包，只干这一件事；
- **HAProxy** 的 `mode tcp`：已经在用 HAProxy，或者想把健康检查和故障转移放在一处时选它；
- **nginx** 的 `stream` 模块：同理，已经有 nginx 时选它。

下面每一段配置都既加密，**又要求客户端出示由你的 CA 签发的证书**。去掉客户端证书那几行，代理就会给任何连上来的人加密。kevy 本身没有 AUTH，这等于谁都能读写所有 key。

### stunnel → 回环端口

```ini
[kevy]
accept      = 0.0.0.0:6379
connect     = 127.0.0.1:6004
cert        = /etc/kevy/tls/server.crt
key         = /etc/kevy/tls/server.key
CAfile      = /etc/kevy/tls/ca.crt
verifyChain = yes
requireCert = yes
```

### HAProxy → unix socket

socket 文件可以用文件系统权限圈起来：kevy 创建的 socket 对所有人可写，所以要把它放进只有终止器所在组能进入的目录，比如权限为 `0750` 的 `/run/kevy`（见 [uds.md](uds.md)）。但这并不意味着终止器成了唯一的入口：下面说的回环 TCP 监听一直开着，在多人共用的主机上，任何本地用户仍然能连 `127.0.0.1:6004`。

```
listen kevy
    bind :6379 ssl crt /etc/kevy/tls/server.pem ca-file /etc/kevy/tls/ca.crt verify required
    mode tcp
    timeout client 0
    timeout server 0
    server kevy unix@/run/kevy/kevy.sock
```

`server.pem` 是证书后面接上私钥合成的一个文件。

带 socket 启动 kevy：

```console
KEVY_UNIX_SOCKET=/run/kevy/kevy.sock kevy --dir /var/lib/kevy
```

这个路径有两点要注意。**路径已经存在时 kevy 拒绝启动**，它不会覆盖一个不是自己创建的路径，所以重启时要先清掉，或者每次用不同的路径。另外 `127.0.0.1` 上的 TCP 监听照旧在，socket 是加上去的，不是替换。

### nginx stream → unix socket

```nginx
stream {
    upstream kevy { server unix:/run/kevy/kevy.sock; }
    server {
        listen 6379 ssl;
        ssl_certificate        /etc/kevy/tls/server.crt;
        ssl_certificate_key    /etc/kevy/tls/server.key;
        ssl_client_certificate /etc/kevy/tls/ca.crt;
        ssl_verify_client      on;
        proxy_pass kevy;
        proxy_timeout 1h;
    }
}
```

注意三段配置里的超时：阻塞中的 `BLPOP` 或者空闲的 Pub/Sub 订阅者，会让连接一直开着但一个字节都不传，应用想开多久就开多久。代理如果会回收空闲连接，看上去就**和 kevy 在丢订阅者一模一样**。nginx 的 `proxy_timeout` 关不掉，要设得比预期最长的空闲时间还长。

## 客户端证书

CA 只需要终止器信任，自建一个就够了。用 OpenSSL：

```console
# CA：ca.key 离线保存
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=kevy-ca" \
  -keyout ca.key -out ca.crt

# 终止器的证书，写客户端连接时用的名字
openssl req -newkey rsa:2048 -nodes -subj "/CN=kevy.internal" -keyout server.key -out server.csr
printf 'subjectAltName=DNS:kevy.internal\n' > san.ext
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 825 \
  -extfile san.ext -out server.crt
cat server.crt server.key > server.pem      # HAProxy 用的格式

# 每个应用一张证书
openssl req -newkey rsa:2048 -nodes -subj "/CN=billing-app" -keyout billing.key -out billing.csr
openssl x509 -req -in billing.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 825 \
  -out billing.crt
```

要吊销某个应用，就换一个新 CA 重新签发，或者给终止器加上 CRL。kevy 本身完全看不到证书。

## 客户端这一侧

任何开了 TLS 的标准 Redis 客户端，都能原样连上终止器后面的 kevy。它需要用 CA 来信任终止器，再出示自己的证书才能进来：

```console
redis-cli --tls --cacert ca.crt --cert billing.crt --key billing.key -h kevy.internal -p 6379 PING
```

**`kevy-cli` 不行。** 它对 `rediss://` 直接报 `Unsupported`：kevy 出厂就没有 TLS，CLI 也没有 TLS 实现可用。这个运维上的后果最好提前安排，别等到用的时候才发现：要么在主机上直接管理，要么走 SSH 隧道：

```console
ssh -N -L 6004:127.0.0.1:6004 you@host   # 然后：kevy-cli -p 6004
```

## 只暴露必要的东西

用默认绑定时，**kevy 一条防火墙规则都不需要**，它本来就没法从主机外访问。要开放的端口只有终止器那一个。如果一定要把 kevy 绑到真实网卡上，防火墙就得接手原本由回环绑定免费完成的工作，而且它会是网络和一个**没有认证的数据库**之间唯一的一道防线。

## 在代理后面用集群模式

按 key 路由的客户端从 `CLUSTER SLOTS` 得知每个 slot 在哪，并跟随 `-MOVED` 重定向，所以 kevy 必须对外公布**代理**监听的地址，而不是自己的。两个配置项负责这件事：

```toml
[cluster]
enabled            = true
announce_ip        = "203.0.113.7"   # 客户端访问代理用的地址
announce_port_base = 7001            # 代理上 shard 0 的端口
```

然后按同样的顺序一一映射每个 shard 的端口：代理的 `7001 + i` 对应 kevy 的 `port + 1 + i`。两个 shard 放在 HAProxy 后面：

```
listen kevy-main
    bind :7000 ssl crt /etc/kevy/tls/server.pem ca-file /etc/kevy/tls/ca.crt verify required
    mode tcp
    server kevy 127.0.0.1:6004
listen kevy-shard0
    bind :7001 ssl crt /etc/kevy/tls/server.pem ca-file /etc/kevy/tls/ca.crt verify required
    mode tcp
    server kevy 127.0.0.1:6005
listen kevy-shard1
    bind :7002 ssl crt /etc/kevy/tls/server.pem ca-file /etc/kevy/tls/ca.crt verify required
    mode tcp
    server kevy 127.0.0.1:6006
```

这样 `CLUSTER SLOTS`、`CLUSTER NODES`、`CLUSTER SHARDS` 和所有 `-MOVED` 里写的都是 `203.0.113.7:7001` 和 `203.0.113.7:7002`。不设 `announce_ip` 时，kevy 公布的是自己的绑定地址，`0.0.0.0` 绑定时公布 `127.0.0.1`：同一台主机上的客户端没问题，从别处就连不上。

## 哪些实测过，哪些没有

在当前代码上实测，终止器用 HAProxy 3.4.5、nginx 1.30.5 和 stunnel 5.76，客户端是 redis-cli 8.0.2，证书由 OpenSSL 3.5.7 签发：

- 上表的端口情况，包括 `--cluster` 为每个 shard 开 `port+1+i`；
- 上面三种客户端证书配置，都转发到 kevy 的 TCP 端口：持有该 CA 签发证书的客户端能读写；不带证书的客户端、以及证书来自另一个 CA 的客户端，在发出命令之前就被断开，它们试图写入的 key 事后查不到；
- HAProxy 和 nginx 两段转发到 kevy 的 unix socket，接受和拒绝的结果同上；kevy 创建的 socket 权限是 `srwxrwxrwx`，所以要靠目录来圈住它。
- 按上面的超时设置，经三种终止器各挂一个 `BLPOP`，空闲 10 分钟后再推入元素放行：三个都拿到了推入的元素；
- 集群模式配合 `announce_ip` 和 `announce_port_base` 放在 HAProxy 后面：`-MOVED` 写的是代理上的 shard 端口，`redis-cli -c` 经一个 shard 端口写入的 key，从主端口全部读得回来；
- 原版 Caddy（2.11.4）不带四层模块；
- `kevy-cli` 拒绝 `rediss://`。

没有测过的：用 CRL 吊销客户端，以及客户端证书过期。

## 参见

- [uds.md](uds.md)：unix socket 的细节
- [cluster.md](cluster.md)：单机集群模式是做什么用的
- [tuning.md](tuning.md)：`--threads`，以及为什么线程少反而可能更快

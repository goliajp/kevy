# 加密链路

复制、选举控制面和客户端连接，都可以各自走 kevy 自带的加密链路，并认证对方身份。它们**默认全部关闭，只有显式开启才生效**。kevy 依然没有 TLS，也没有 AUTH：需要 TLS 的客户端按 [deploy-behind-a-proxy.md](deploy-behind-a-proxy.md) 的做法经代理接入；开不开加密端口，明文客户端端口的行为都不变。

## 覆盖哪些链路

| 链路 | 何时加密 | 谁被认证 |
|---|---|---|
| 选举（`elect_port_base`） | `[cluster] secure = true` | 两端都认证，依据 `peer_keys` 里的公钥 |
| 复制（`listen_port_base + i`） | `[replication] secure = true` | 主节点凭 `upstream_key` 或某个 peer 公钥认证；设置了 `replica_keys` 时 replica 也被认证 |
| `[secure] listen_port` 上的客户端 | 始终加密 | 服务端凭自己的公钥认证；设置了 `client_keys` 时客户端也被认证 |
| `port`、集群端口、unix socket 上的客户端 | 不加密 | 不认证，请用代理 |

协议是 Noise `IK`，用 X25519、ChaCha20-Poly1305 和 BLAKE2s。发起方连接前就知道对方的公钥；响应方从第一条消息里得知发起方的公钥，在回复之前就可以拒绝它；这一个来回之后的所有内容都加密并带认证。这些密码学原语是 kevy 自己实现的，没有任何依赖，已经对照公开的测试向量和其他实现做过验证，但没有经过第三方审计。

## 密钥

每个节点需要一对密钥。`kevy keygen` 写出私钥，并打印公钥：

```console
$ kevy keygen /etc/kevy/node.key
ffe2a40f453275a19e8b115332459968e9a3b54b9c2e728ec538ccdd724b2f6c
```

私钥文件以 `0600` 权限创建，已存在时不会被覆盖。私钥文件如果其他用户可读，kevy 拒绝启动。

## 配置

每个节点写明自己的私钥，并列出所有对端的公钥，即各节点上 `kevy keygen` 打印出来的那一行。下面是 `n2` 的配置：

```toml
[secure]
private_key_file = "/etc/kevy/node.key"

[cluster]
enabled   = true
node_id   = "n2"
secure    = true
peers     = "n1@10.0.0.11:6204:6004,n2@10.0.0.12:6204:6004,n3@10.0.0.13:6204:6004"
peer_keys = ["n1=d5c015b88401b6b33f5cb292b01ff3034e5f1de008b2e3f77faed23c31f16a4c", "n3=38bde379dfddf094d8746267013da2d5b62153c74dea6f82a4910797faecb440"]

[replication]
role         = "replica"
upstream     = "10.0.0.11:16004"
secure       = true
upstream_key = "d5c015b88401b6b33f5cb292b01ff3034e5f1de008b2e3f77faed23c31f16a4c"
replica_keys = []   # 在主节点上：允许连接的 replica 公钥；留空表示都允许
```

- `peers` 里除自己之外的每个节点，`peer_keys` 都要有一项；自己那一项会被忽略，所以所有节点可以共用同一份列表。
- replica 把 `upstream_key` 和 `peer_keys` 里的每个公钥都当作可信的主节点。选举切换后，它不用改任何配置就能跟随新主，因为新主的公钥本来就在列表里。
- 主节点上的 `replica_keys` 限定哪些 replica 可以连接。留空时任何 replica 都能连，链路照样加密。

kevy 在启动时检查这些配置：开了 `secure = true` 却没有 `private_key_file`、某个对端没有公钥、或者加密的 replica 没有 `upstream_key`，节点都会停止启动，并说明缺了什么。它不会退回明文。

## 嵌入式存储

嵌入式写端和它的 replica 用同样的方式加密链路，只是在代码里配置，不用 `kevy.toml`。握手和 server 完全相同，所以嵌入式 replica 可以跟随加密的 kevy server，server replica 也可以跟随加密的嵌入式写端。

```rust
use kevy_embedded::{Config, Keypair, LinkKeys, Store};

# fn keys() -> (Keypair, Keypair) { (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32])) }
let (writer_key, replica_key) = keys();
let writer_public = writer_key.public();

// 写端：任何 replica 都能连，链路照样加密
let writer = Store::open(
    Config::default()
        .with_embed_writer("0.0.0.0:7101")
        .with_writer_security(LinkKeys { local: writer_key, peers: vec![] }),
)?;

// replica：信任写端的公钥
let replica = Store::open(
    Config::default()
        .without_aof()
        .with_replica_upstream("writer.internal:7101")
        .with_replica_security(LinkKeys { local: replica_key, peers: vec![writer_public] }),
)?;
# Ok::<(), kevy_embedded::KevyError>(())
```

- 在 replica 上，`peers` 列出它信任的主节点，逐个尝试直到有一个应答；列表为空时打开会失败。在写端上，它列出允许连接的 replica；留空表示都允许。
- `Keypair::from_secret` 接收的 32 字节，就是 `kevy keygen` 以 hex 写出的那一串；应用把它存在哪里由应用自己决定。

## 客户端

`[secure] listen_port` 开一个只讲加密协议的第二个客户端端口，明文的 `port` 保持原样。客户端用 `kevys://` URL 连接，URL 里写明服务端的公钥：

```toml
[secure]
private_key_file = "/etc/kevy/node.key"
listen_port      = 6404
client_keys      = []   # 允许的客户端公钥；留空表示都允许，链路照样加密
```

```text
kevys://10.0.0.11:6404?server_key=d5c015b88401b6b33f5cb292b01ff3034e5f1de008b2e3f77faed23c31f16a4c
kevys://10.0.0.11:6404/0?server_key=<hex>&client_key_file=/etc/app/kevy.key
```

- 不带 `client_key_file` 的客户端，每条连接各自生成一对新密钥。`client_keys` 为空时这就够了；不为空时，用 `kevy keygen` 为客户端生成密钥，并把公钥列进去。
- 接受 `kevys://` 的是 Rust 客户端：`kevy-resp-client`（`RespClient::connect_url`，或者把 `SecureStream` 放在任何 RESP 代码下面）、`kevy-client`（`Connection` 和 `Subscriber`）、`kevy-client-async`（`AsyncConnection::connect_secure_url`、`AsyncSubscriber::connect_secure_url`）。`kevy-cli -u` 也接受。其他语言的绑定不支持，请用 TLS 代理。
- `CLIENT LIST`、`CLIENT INFO` 和 `CLIENT KILL ADDR` 显示的是客户端自己的地址，不是服务端的。
- 这个端口需要 `private_key_file`，并且不能和 `port` 相同；两种配置错误都会让服务端在启动时停下。

## 代价

加解密在 reactor 旁边的独立线程上完成：每条连接的字节在那里解密，经本机回环交给明文端口，回复在返回途中加密。开不开这个端口，明文路径都是同一套代码；加密连接除了加解密本身，还要多付一次本机往返。

在一台 Linux 主机上实测，客户端和服务端走本机回环，4 个 shard：

| | 明文端口 | 加密端口 |
|---|---:|---:|
| 单个请求的往返 | 10 µs | 25 µs |
| 单连接 256 KB `GET` | 3.0 GB/s | 0.32 GB/s |
| 新建连接加一次 `PING` | 38 µs | 0.58 ms |

往返多出的时间大部分是那一跳；小消息的加密和解密合计约半微秒。大值受限于加密算法，它是一份可移植的实现；握手的耗时主要在 X25519。连接池会一直保持连接，握手只付一次。

## 加密节点和明文节点混用

加密节点不和明文节点通信：明文 replica 连加密主节点得不到任何回复；公钥不在其他节点 `peer_keys` 里的节点，在选举里没人听得到。要把一个集群切到加密，用新配置把每个节点都重启一遍。

## 哪些实测过

三个节点、每个节点一个 shard，选举和复制都加密：

- 主节点上的写入到达了两个 replica；节点之间的网络抓包里，写了 200 次的标记值在复制端口和选举端口上一次都没出现；同样的步骤在 `secure = false` 下出现了 400 次；
- 杀掉主节点后选出了新主，另一个节点跟随它，并收到了切换之后的写入；
- 每节点 4 个 shard、使用 io_uring reactor 时，受 `replica_keys` 限制的 replica 收到了全部 400 个 key，复制端口的抓包里一个值都没有；同样配置的明文运行里 400 个全部可见；
- 期待另一个主节点公钥的 replica、不在 `replica_keys` 里的 replica、明文 replica，以及持有未配置公钥的选举对端，都被拒绝了。

## 参见

- [deploy-behind-a-proxy.md](deploy-behind-a-proxy.md)：用不了 `kevys://` 的客户端怎么上 TLS
- [replication.md](replication.md)：复制本身
- [availability.md](availability.md)：选举与故障切换

# 非同期クライアント(`kevy-client-async`)

`kevy-client-async` は [`kevy-client`](https://docs.rs/kevy-client)
の runtime-agnostic な非同期版です。ブロッキング・クライアントは新規
コードのデフォルトのまま(純 Rust、ゼロ依存、非同期でないワークロード
ではレイテンシも低い)。この crate は、既に `tokio` / `smol` /
`async-std` runtime を持っていて `await` フローを貫通させたいアプリ
向け —— 特にパイプライニング、ここが async で N round-trip を 1 回に
畳める場所です。

## どれを使うか

| 状況 | 選ぶ |
|------|------|
| runtime なし、シンプルな request-response | `kevy-client` |
| tokio アプリ、コマンド毎に 1 `await` | `kevy-client-async` |
| tokio アプリ、バッチ毎に 1 `await` | `kevy-client-async` + `pipeline()` |
| 任意の runtime で組込み `mem://` / `file://` | `kevy-client` |

`AsyncConnection::open` は `mem://` と `file://` URL を拒否します ——
それらはプロセス内同期バックエンドで、ブロッキング・クライアントの方
が厳密に速いからです。

## Runtime 選択

`tokio` / `smol` / `async-std` の **ちょうど 1 つ** を有効化する必要
があります。ゼロ個または 2 つ以上を有効化するとコンパイル時エラーに
なります。

```toml
[dependencies]
kevy-client-async = { version = "1", features = ["tokio"] }
```

各 runtime は自分の `TcpStream` アダプタを持ちます:

| feature | トランスポート |
|---------|--------------|
| `tokio` | `tokio::net::TcpStream` |
| `smol`  | `smol::net::TcpStream` |
| `async-std` | `async_std::net::TcpStream` |

各 runtime 依存は `default-features = false` + アダプタが必要とする最
小限の feature だけを取り込みます。

## サーフェス —— ブロッキングのミラー

```rust
use kevy_client_async::AsyncConnection;

let mut conn = AsyncConnection::open("tcp://127.0.0.1:6004").await?;
conn.set(b"k", b"v").await?;
let v = conn.get(b"k").await?;
```

`AsyncConnection` のメソッド名は `kevy_client::Connection` と 1:1
(`.await` が付くだけ)。ブロッキングからの移行は `Connection` →
`AsyncConnection` の grep-replace と、各呼び出しへの `.await` 追加です。

利用可能なコマンド・ファミリ(42 メソッド):

- **string + generic**:ping / set / get / del / exists / incr /
  incr_by / expire / persist / ttl_ms / type_of / dbsize / flushall /
  set_with_ttl / mget / mset / publish
- **hash**:hset / hget / hdel / hlen / hgetall / hkeys / hvals
- **list**:lpush / rpush / lpop / rpop / llen / lrange
- **set**:sadd / srem / smembers / scard / sismember / sinter /
  sunion / sdiff
- **sorted set**:zadd / zrem / zscore / zcard / zrange

## Pipeline-first シュガー

async が本当に成果を出すのはここ —— バッチ毎に 1 回のネットワーク
round-trip で、コマンド毎ではありません。

```rust
let replies = conn
    .pipeline()
    .set(b"k1", b"v1")
    .get(b"k2")
    .incr(b"counter")
    .run(&mut conn)
    .await?;
// replies: Vec<Reply>、enqueue 順に各コマンド 1 エントリ。
```

コマンド単位のエラーは返却された `Vec` の中で `Reply::Error(_)` と
して現れます —— 1 つの不正コマンドがバッチ全体を壊しません。外側の
`Err` は接続レベルの失敗(トランスポート、不正フレーム)向けに予約
されています。

タイプ付きビルダーにないコマンドには `push_raw(argv)` を:

```rust
conn.pipeline()
    .push_raw(vec![b"CUSTOM".to_vec(), b"arg".to_vec()])
    .run(&mut conn).await?;
```

### 降格パス

`Pipeline::into_cmds()` は `Vec<Vec<Vec<u8>>>` を返します —— 生の argv
バッチ。フォールバックでブロッキング・クライアントに 1 つずつ送り込み
たい場合に:

```rust
let cmds = conn.pipeline().get(b"a").set(b"b", b"v").into_cmds();
// ブロッキング kevy_client::Connection 上で:
// for cmd in &cmds { blocking_conn.codec_mut().request(cmd)?; }
```

## Cluster client

`AsyncClusterClient` は cluster モード・サーバー向けに
`kevy_client::ClusterClient` をミラーします —— shard 毎に 1 TCP 接続、
key 毎に CRC16 ルーティング、正しいルーティング下では `-MOVED` は発火
しません。

```rust
use kevy_client_async::cluster::AsyncClusterClient;

let mut c = AsyncClusterClient::connect("127.0.0.1", 6004).await?;
c.set(b"user:42", b"…").await?;
```

## Subscriber

`AsyncSubscriber` は `kevy_client::Subscriber` をミラーします ——
subscribe 済みの RESP 接続は通常コマンドを送れないため、
`AsyncConnection` とは別の型です。ブロッキング形状の drop-in、
ただし socket 級 `set_read_timeout` は外しています(runtime のタイム
アウト原語を使ってください:`tokio::time::timeout`、`async_io::Timer`
など)。

```rust
use kevy_client_async::subscriber::AsyncSubscriber;

let mut sub = AsyncSubscriber::open("tcp://127.0.0.1:6004", &[b"ch"]).await?;
let (channel, payload) = sub.recv_message().await?;
```

## エラー

各 async メソッドは `std::io::Result<T>` を返し、ブロッキング・
クライアントと同じ `ErrorKind` マッピングを使います:

| 出典 | `ErrorKind` |
|------|------------|
| RESP `-ERR …` 応答 | `Other` |
| 想定外の応答 variant | `Other` |
| 不正な RESP フレーム | `InvalidData` |
| 読み込み途中の EOF | `UnexpectedEof` |
| 不正な URL / ポート / scheme | `InvalidInput` |
| TLS / AUTH / embed URL scheme | `Unsupported` |
| 生 socket I/O | (native kind) |

より広いエラー・コンテキスト —— RESP エラー文字列、想定外の variant
名 —— は `io::Error` の message にあります(`.to_string()` /
`.into_inner()`)。

## 依存規則の例外

`kevy-client-async` は kevy workspace の中で crates.io 依存を取って
よい**唯一の** crate です。例外は crate 単位 + dep 単位:`tokio`、
`smol`、`async-std` だけが取り込まれる crate(各々 `Cargo.toml` に
インラインの `# EXEMPTION` コメントを持つ)。他の workspace crate
は `kevy-client-async` を依存に取ってはいけません —— それは例外を
推移的に漏らしてしまいます。完全な根拠は v3-cluster RFC(F5)と
`feedback-pure-rust-no-c-principle.md` memory にあります。

## サンプル

- [`tokio_hello`](../../crates/kevy-client-async/examples/tokio_hello.rs)
  —— open + ping + set/get + del。
- [`pipeline`](../../crates/kevy-client-async/examples/pipeline.rs)
  —— 1 round-trip で混合バッチを実行。

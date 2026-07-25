# 非同期クライアント

`kevy-client-async`は、ブロッキング版[`kevy-client`](https://github.com/goliajp/kevy/tree/develop/crates/kevy-client)の非同期ミラーです。API面もURLファサードも同じで、各呼び出しに`.await`が付くだけです。

## このドキュメントが必要になるとき

アプリがすでに`tokio`、`smol`、`async-std`のいずれかのランタイム上で動いていて、`await`のフローを端から端まで通したい（ブロッキングスレッドプールへのホップ、`spawn_blocking`によるラップ、コネクションごとのスレッドをなくしたい）場合に、非同期クライアントを使ってください。コードパスが通常スレッド上のリクエスト・レスポンスであれば、ブロッキングクライアントのほうがシンプルかつ低遅延です。同期でいることに非同期税はかかりません。

## 中心となる考え方

Cargoフィーチャでランタイム（`tokio`、`smol`、`async-std`）をちょうど1つ選びます。クレートはそのランタイムの`TcpStream`アダプタだけにコンパイルされ、ほかは一切含まれません。公開APIはブロッキングクライアントを1:1でミラーしているため（`AsyncConnection::connect(url).await?`、`conn.set(k, v).await?`、`conn.get(k).await?`）、ブロッキングからの移植は`Connection`を`AsyncConnection`に替えて各呼び出しに`.await`を付けるだけで済みます。レイテンシが問題になる場面では、pipelineビルダーがNコマンドを1回のTCP往復にまとめます。

## 動かしてみる例

### Tokio

```toml
[dependencies]
kevy-client-async = { version = "1", features = ["tokio"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "net"] }
```

```rust
use kevy_client_async::AsyncConnection;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut conn = AsyncConnection::connect("tcp://127.0.0.1:6004").await?;
    conn.set(b"k", b"v").await?;
    let v = conn.get(b"k").await?;
    assert_eq!(v.as_deref(), Some(&b"v"[..]));
    Ok(())
}
```

### Smol

コードは同じで、ランタイムのフィーチャだけ入れ替えます。

```toml
[dependencies]
kevy-client-async = { version = "1", features = ["smol"] }
smol = "2"
```

```rust
use kevy_client_async::AsyncConnection;

fn main() -> std::io::Result<()> {
    smol::block_on(async {
        let mut conn = AsyncConnection::connect("tcp://127.0.0.1:6004").await?;
        conn.set(b"k", b"v").await?;
        let v = conn.get(b"k").await?;
        assert_eq!(v.as_deref(), Some(&b"v"[..]));
        Ok(())
    })
}
```

### Pipelineビルダー

バッチ全体を1往復で送ります。応答はキューに入れた順に返り、コマンド単位の失敗はバッチを壊さず、`Vec`の中に`Reply::Error(_)`として現れます。

```rust
use kevy_client_async::AsyncConnection;

let mut conn = AsyncConnection::connect("tcp://127.0.0.1:6004").await?;
let replies = conn
    .pipeline()
    .set(b"a", b"1")
    .get(b"a")
    .incr(b"hits")
    .run(&mut conn)
    .await?;
// replies.len() == 3; キューしたコマンドごとに1つのReply、順序どおり。
```

## ランタイムフィーチャ

以下のうちちょうど1つを有効にする必要があります。フィーチャなし、あるいは2つ以上の同時有効はコンパイルエラーになります。暗黙のデフォルトはありません。

| フィーチャ    | トランスポートアダプタ              | 取り込まれるランタイムクレート |
|--------------|------------------------------------|----------------------|
| `tokio`      | `tokio::net::TcpStream`            | `tokio`              |
| `smol`       | `smol::net::TcpStream`             | `smol`               |
| `async-std`  | `async_std::net::TcpStream`        | `async-std`          |

各ランタイムクレートは`default-features = false`に、アダプタに必要な最小限のフィーチャだけを足して取り込まれます。kevyワークスペースのcrates.io依存はこれらだけです。純Rust・ゼロ依存ルールに対する意図的な例外であり、Rustのasyncエコシステムにはstdだけで成立する基盤が存在しないための措置です。

## URLバックエンド

`AsyncConnection::connect`はブロッキングクライアントと同じURLファサードを受け取ります。TCP系のスキームはランタイムの非同期ソケットを通り、プロセス内スキームは拒否されます（プロセス内アクセスはブロッキングクライアントのほうが厳密に速く、エグゼキュータを経由させる意味がないためです）。

| スキーム     | 接続先                              | 非同期クライアントの対応 |
|--------------|---------------------------------|---------------------------|
| `tcp://`     | kevyまたはRedis互換サーバー     | 対応                       |
| `kevy://`    | kevyサーバー（`tcp://`のエイリアス） | 対応                       |
| `redis://`   | RedisまたはRedis互換サーバー    | 対応                       |
| `mem://`     | プロセス内組み込みストア       | 非対応（ブロッキング版を使用）  |
| `file:///`   | オンディスク組み込みストア          | 非対応（ブロッキング版を使用）  |

`mem://`や`file:///`のURLを`AsyncConnection::connect`で開くと`ErrorKind::Unsupported`が返ります。

## トレードオフ

ブロッキングクライアントがデフォルトであり、今後もデフォルトであり続けるのには理由があります：

- **同期コードパス**：ランタイムをまだ持っていないなら、クライアントのためだけに立ち上げるべきではありません。`kevy-client`は純Rust・ゼロ依存で、コマンドごとにエグゼキュータのスケジューリングオーバーヘッドを払うこともありません。
- **組み込みバックエンド**：`mem://`と`file:///`は同期のプロセス内ストアです。ブロッキングクライアントは直接話せますが、非同期クライアントからは使えません。
- **単発コマンド**：一般的なマルチスレッドエグゼキュータでは、コマンドごとの`.await`1回でも、直接のsyscallと比べれば計測できる程度のオーバーヘッドになります。非同期の利得が見えるのは、並行性（タスクをまたいだ多数のin-flightコマンド）やバッチング（pipelineによる往復の集約）がある場面です。

周囲のアプリがすでにasyncならasyncを使う。独立したコマンドのバッチがあって往復がボトルネックならpipelineビルダーを使う。それ以外はブロッキングのままにしておいてください。

## FAQ

**なぜランタイムをちょうど1つ選ぶ必要があるのですか？**
クレートは単一の`TcpStream`アダプタとしてコンパイルされます。1つのバイナリにアダプタを2つ入れると、I/Oごとにランタイム非依存の間接層を挟む（オーバーヘッド）か、誰にも保守できない巨大なcfgマトリクスを抱えるかのどちらかになります。アダプタ0個では公開型が未実装のまま残ります。フィーチャ数をコンパイル時に検査することで、設定ミスを早い段階ではっきり検出できます。

**同期と非同期のkevyクライアントを1つのプロセスに混在させられますか？**
はい。`kevy-client`（ブロッキング）と`kevy-client-async`は独立したクレートで、自由に共存できます。たとえば同じバイナリの中で、組み込みの`file:///`ストアにはブロッキング版を、ネットワークシャードにはasync版を、という使い分けが可能です。コネクションは共有されません。

**pub/subはどうなりますか？**
`AsyncSubscriber`がブロッキングの`Subscriber`をミラーします。購読状態のRESPコネクションは通常のコマンドを送れないため、`AsyncConnection`とは別の型になっています。メッセージ単位のタイムアウトには、ソケットレベルのreadタイムアウトではなく、使っているランタイム自身のプリミティブ（`tokio::time::timeout`、`async_io::Timer`など）を使ってください。

**pipelineビルダーは送信側のバッファリングを強制しますか？**
はい。それこそが狙いです。`pipeline().…run(&mut conn).await`はバッチ全体を1回のwriteにシリアライズし、N個の応答を順に読み取ります。コマンド単位のバックプレッシャが必要なら、pipelineを組まずに`set`/`get`を直接呼んでください。

## リポジトリ内のサンプル

- [`tokio_hello`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-client-async/examples/tokio_hello.rs) — オープン、ping、set/get、del。
- [`pipeline`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-client-async/examples/pipeline.rs) — 混在バッチを1往復で。

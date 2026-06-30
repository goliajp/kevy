# 非同期クライアント

`kevy-client-async` はブロッキング版 [`kevy-client`](https://github.com/goliajp/kevy/tree/develop/crates/kevy-client) の非同期ミラーです — 同じ面、同じ URL ファサード、各呼び出しに `.await` が付きます。

## このドキュメントが必要になるとき

アプリが既に `tokio`、`smol`、`async-std` ランタイム上で動いていて、`await` フローを端から端まで通したい(ブロッキングスレッドプールへのホップなし、`spawn_blocking` ラップなし、コネクションごとスレッドなし)ときに非同期クライアントを使ってください。コードパスが通常スレッド上のリクエスト・レスポンスなら、ブロッキングクライアントの方がシンプルで低遅延です — 同期コードに非同期税はかかりません。

## 中心となる考え方

Cargo 機能で正確に 1 つのランタイム(`tokio`、`smol`、`async-std`)を選びます。クレートはそのランタイムの `TcpStream` アダプタにだけコンパイルされ、他は含まれません。公開面はブロッキングクライアントを 1:1 でミラーします — `AsyncConnection::open(url).await?`、`conn.set(k, v).await?`、`conn.get(k).await?` — なので、ブロッキングからの移植は `Connection` → `AsyncConnection` と各呼び出しへの `.await` です。遅延が問題のときは pipeline builder が N コマンドを 1 回の TCP 往復に畳みます。

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
    let mut conn = AsyncConnection::open("tcp://127.0.0.1:6004").await?;
    conn.set(b"k", b"v").await?;
    let v = conn.get(b"k").await?;
    assert_eq!(v.as_deref(), Some(&b"v"[..]));
    Ok(())
}
```

### Smol

同じコード。ランタイム機能だけ入れ替えます。

```toml
[dependencies]
kevy-client-async = { version = "1", features = ["smol"] }
smol = "2"
```

```rust
use kevy_client_async::AsyncConnection;

fn main() -> std::io::Result<()> {
    smol::block_on(async {
        let mut conn = AsyncConnection::open("tcp://127.0.0.1:6004").await?;
        conn.set(b"k", b"v").await?;
        let v = conn.get(b"k").await?;
        assert_eq!(v.as_deref(), Some(&b"v"[..]));
        Ok(())
    })
}
```

### Pipeline builder

バッチ全体で 1 往復。応答はキュー順に返り、コマンドごとの失敗はバッチを破壊せず `Vec` 内に `Reply::Error(_)` として着地します。

```rust
use kevy_client_async::AsyncConnection;

let mut conn = AsyncConnection::open("tcp://127.0.0.1:6004").await?;
let replies = conn
    .pipeline()
    .set(b"a", b"1")
    .get(b"a")
    .incr(b"hits")
    .run(&mut conn)
    .await?;
// replies.len() == 3; キューしたコマンドごとに 1 つの Reply、順序保持。
```

## ランタイム機能

これらのうちちょうど 1 つが有効でなければなりません。ゼロ機能、あるいは 2 つ以上同時はコンパイル時エラーです — 暗黙のデフォルトはありません。

| 機能      | トランスポートアダプタ                  | 引かれるランタイムクレート |
|--------------|------------------------------------|----------------------|
| `tokio`      | `tokio::net::TcpStream`            | `tokio`              |
| `smol`       | `smol::net::TcpStream`             | `smol`               |
| `async-std`  | `async_std::net::TcpStream`        | `async-std`          |

各ランタイムクレートは `default-features = false` で、アダプタが必要な最小機能だけを付けて引かれます。これらは kevy ワークスペースで唯一の crates.io 依存です — 純粋 Rust・ゼロ依存ルールに対する意図的な切り出しです。Rust の async エコシステムには std だけで実現可能な基盤がないためです。

## URL バックエンド

`AsyncConnection::open` はブロッキングクライアントと同じ URL ファサードを取ります。TCP 形状のスキームはランタイムの非同期ソケットを通り、プロセス内スキームは拒否されます(ブロッキングクライアントの方がそれらには厳密に速いので、エグゼキュータ経由にする意味がありません)。

| スキーム       | ターゲット                          | 非同期クライアントでサポート |
|--------------|---------------------------------|---------------------------|
| `tcp://`     | kevy または Redis 互換サーバー     | はい                       |
| `kevy://`    | kevy サーバー(`tcp://` のエイリアス) | はい                       |
| `redis://`   | Redis または Redis 互換サーバー    | はい                       |
| `mem://`     | プロセス内組み込みストア       | いいえ — ブロッキングクライアントを  |
| `file:///`   | オンディスク組み込みストア          | いいえ — ブロッキングクライアントを  |

`AsyncConnection::open` で `mem://` や `file:///` URL を開くと `ErrorKind::Unsupported` を返します。

## トレードオフ

ブロッキングクライアントがデフォルトで、これからもデフォルトであるのには理由があります:

- **同期コードパス**: まだランタイムがなければ、クライアントのためにランタイムを立てないでください。`kevy-client` は純粋 Rust・ゼロ依存で、各コマンドでエグゼキュータのスケジューリングオーバーヘッドを避けます。
- **組み込みバックエンド**: `mem://` と `file:///` は同期のプロセス内ストアです。ブロッキングクライアントは直接話せます。非同期クライアントは話せません。
- **シングルショットコマンド**: 通常のマルチスレッドエグゼキュータでコマンドごとの `.await` 1 つは、直接 syscall と比べて計測可能なオーバーヘッドです。非同期の利得は並行性(タスクをまたぐ多数の in-flight コマンド)やバッチング(往復を畳む pipeline)で見えます。

周囲のアプリが既に async なら async を使ってください。独立したコマンドのバッチがあり往復がボトルネックなら pipeline builder を使ってください。それ以外はブロッキングのままで。

## FAQ

**なぜランタイムを正確に 1 つ選ぶ必要がありますか?**
クレートは 1 つの `TcpStream` アダプタにコンパイルされます。1 バイナリ内に 2 つのアダプタを入れると、各 I/O ごとのランタイム非依存な間接化(オーバーヘッド)か、誰も保守できない巨大な cfg マトリクスのどちらかになります。ゼロアダプタは公開型を未実装にします。機能数のコンパイル時チェックが、設定ミスを大きく早く拾います。

**同期と非同期の kevy クライアントを 1 プロセスで混ぜられますか?**
はい。`kevy-client`(ブロッキング)と `kevy-client-async` は独立したクレートで自由に共存します — 例えば同じバイナリで組み込みの `file:///` ストアにはブロッキング、ネットワークシャードには async、と使えます。コネクションは共有しません。

**pub/sub はどうですか?**
`AsyncSubscriber` がブロッキングの `Subscriber` をミラーします。subscribed な RESP コネクションは通常コマンドを送れないので、`AsyncConnection` とは別の型です。メッセージごとのタイムアウトはソケットレベルの read タイムアウトではなく、あなたのランタイム自身のプリミティブ(`tokio::time::timeout`、`async_io::Timer` 等)を使ってください。

**pipeline builder は送信側のバッファリングを強制しますか?**
はい — それが要点です。`pipeline().…run(&mut conn).await` はバッチ全体を 1 回の write にシリアライズし、N 応答を順序で読みます。コマンド単位のバックプレッシャが必要なら、pipeline を組まずに `set` / `get` を直接呼んでください。

## リポジトリ内のサンプル

- [`tokio_hello`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-client-async/examples/tokio_hello.rs) — 開く、ping、set/get、del。
- [`pipeline`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-client-async/examples/pipeline.rs) — 混在バッチを 1 往復で。

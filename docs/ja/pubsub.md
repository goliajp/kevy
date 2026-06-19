# kevy-client の Pub/sub

同じコードがプロセス内バスと TCP kevy サーバーの両方を駆動。実行時
に URL でバックエンドを選ぶ —— 呼び出し現場で scheme 分岐は不要。

```toml
[dependencies]
kevy-client = "1.11"
```

## URL セマンティクス

| URL | バックエンド | open をまたいで共有? |
|-----|------------|------------------|
| `mem://` | プロセス内、インメモリ | **いいえ** —— open 毎に新規 |
| `mem://<name>` | プロセス内、インメモリ | **はい** —— 同 `<name>` → 同バス |
| `file:///abs/path` | プロセス内 + snapshot/AOF 永続化 | **はい** —— 同パス → 同バス |
| `kevy://host[:port][/db]` | TCP kevy/Redis サーバー | (open 毎に 1 ソケット、サーバー側で扇出) |
| `redis://host[:port][/db]` | TCP —— `kevy://` のエイリアス | 同 |
| `tcp://host[:port]` | TCP —— 生、先頭 `SELECT` なし | 同 |

`rediss://` / `kevys://` / `redis://user:pass@…` は
`ErrorKind::Unsupported` で拒否 —— kevy は TLS / AUTH なしで出荷。

**匿名 `mem://` は publish されたメッセージを受信できません** ——
同じバッキング `Store` に到達できる他のものがないからです。
`Subscriber::open` は `ErrorKind::Unsupported` でそれを拒否します。
`mem://<some-name>` を使ってください。

**クラスタ注記**:kevy の pub/sub は **プロセス・レベル**、slot
ルーティングではありません:任意の cluster shard ポートでの publish
は、同プロセス内の他の shard ポートの subscriber に到達します。
pub/sub に `ClusterClient` は **不要** —— 任意の shard ポートへの
普通の `Connection::open("kevy://host:port")` で動作します。slot
ルーティングされた keyspace トラフィックについては
[`docs/cluster.md`](cluster.md) を参照。

## Pattern 1 —— 同一スレッドの dev ループ

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

let mut sub  = Subscriber::open("mem://app", &[b"news"])?;
let mut conn = Connection::open("mem://app")?;

// publish 前に SUBSCRIBE ack をドレイン —— bus は順序付き、ack は
// キュー内の最初の Message の前に到着します。
let _ack = sub.recv()?;

conn.publish(b"news", b"hello")?;

if let PubsubEvent::Message { channel, payload } = sub.recv()? {
    assert_eq!(channel, b"news");
    assert_eq!(payload, b"hello");
}
# Ok::<(), std::io::Error>(())
```

## Pattern 2 —— クロス・スレッド producer / consumer

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};
use std::thread;

const URL: &str = "mem://orders";

let mut sub = Subscriber::open(URL, &[b"order.placed"])?;
let _ack = sub.recv()?;

thread::spawn(|| {
    let mut conn = Connection::open(URL).unwrap();
    conn.publish(b"order.placed", b"order-42").unwrap();
});

let ev = sub.recv()?;
// PubsubEvent::Message { channel: "order.placed", payload: "order-42" }
# Ok::<(), std::io::Error>(())
```

## Pattern 3 —— 環境駆動の dev/prod スワップ

同コード、3 バックエンド:

```rust
use kevy_client::{Connection, Subscriber};

fn run_app(url: &str) -> std::io::Result<()> {
    let mut sub  = Subscriber::open(url, &[b"jobs"])?;
    let mut conn = Connection::open(url)?;
    let _ack = sub.recv()?;
    conn.publish(b"jobs", b"compute pi")?;
    // ... events をドレイン ...
    Ok(())
}

// Dev:
run_app("mem://app")?;
// 永続化付きテスト:
run_app("file:///tmp/app-test")?;
// Prod:
run_app("kevy://prod-cache:6379")?;
# Ok::<(), std::io::Error>(())
```

呼び出し現場に `match scheme { ... }` なし。1 つの URL を open、両端
が同じバッキング・バスにアタッチします。

## Pattern 4 —— glob パターン

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

let mut sub = Subscriber::connect("mem://signals")?;
sub.psubscribe(&[b"sensor.*"])?;
let _ack = sub.recv()?;  // Psubscribe ack

let mut conn = Connection::open("mem://signals")?;
conn.publish(b"sensor.temp", b"22.5")?;  // マッチ
conn.publish(b"weather", b"sunny")?;     // マッチ **しない**

if let PubsubEvent::Pmessage { pattern, channel, payload } = sub.recv()? {
    assert_eq!(pattern, b"sensor.*");
    assert_eq!(channel, b"sensor.temp");
    assert_eq!(payload, b"22.5");
}
# Ok::<(), std::io::Error>(())
```

Glob 構文:`*`(任意)、`?`(1 文字)、`[abc]`(文字クラス) ——
`KEYS` / `SCAN` と同じ matcher。

## Pattern 5 —— 複数 subscriber への扇出

```rust
use kevy_client::{Connection, Subscriber};

const URL: &str = "mem://fanout";
let mut s1 = Subscriber::open(URL, &[b"chan"])?;
let mut s2 = Subscriber::open(URL, &[b"chan"])?;
let _ = s1.recv()?;
let _ = s2.recv()?;

let mut conn = Connection::open(URL)?;
let received = conn.publish(b"chan", b"broadcast")?;
assert_eq!(received, 2);  // 両方とも受信
# Ok::<(), std::io::Error>(())
```

## API 要約

```rust
// Producer
let mut conn = Connection::open(url)?;
let recv_count = conn.publish(channel, payload)?;

// Consumer
let mut sub = Subscriber::open(url, &[channel])?;          // open + subscribe
// または
let mut sub = Subscriber::connect(url)?;                    // open、後で subscribe
sub.subscribe(&[chan1, chan2])?;
sub.psubscribe(&[b"foo.*"])?;
sub.unsubscribe(&[chan1])?;       // 空 &[] → 全 channel 解除
sub.punsubscribe(&[])?;            // 空 &[] → 全 pattern 解除
sub.set_read_timeout(Some(Duration::from_secs(1)))?;
let ev: PubsubEvent = sub.recv()?;
```

`PubsubEvent` は 6 つの variant を持ちます:`Subscribe`、`Psubscribe`、
`Unsubscribe`、`Punsubscribe`、`Message`、`Pmessage`。`Unsubscribe` /
`Punsubscribe` は channel/pattern スロットに `Option<Vec<u8>>` を使い
ます —— `None` は "subscribed なし" の nil-bulk ワイヤ形状にマッチ。

## ライフサイクル + 落とし穴

**プロセス・ローカル・レジストリ**:URL → `Store` マップはプロセス
毎、`Weak` 参照でバックされています。名前付き URL の最後の
`Connection` / `Subscriber` が drop すると entry が解放;同 URL の次
の open は新しい `Store` を取得します。(`file:///` URL ではディスク
の AOF + snapshot は残り、re-open で replay。)

**クロス・プロセス**:`mem://name` と `file:///path` は他のプロセス
から **見えません**。本物のクロス・プロセス配信には、kevy サーバーを
起動して `kevy://host:port` を使用。

**Ack 順序**:`SUBSCRIBE` はそのチャネルの任意の `Message` の前に
`Subscribe` ack を受信キューに enqueue します。テストでメッセージ本体
を assert する前に ack をドレインしてください。

**送信タイミング**:bus mutex は `Sender::send()` 呼び出しの前に
drop されるため、遅い receiver が無関係のチャネルへの publish を
stall させることはできません。各 subscriber は自分の `mpsc::Receiver`
キューを持ちます(共有 bound なし)。

**`Subscription` drop はアトミックに登録解除**:スレッドがパニック
しても "stale subscriber" zombie 状態は残りません —— `Drop` impl が
bus テーブルを walk して subscription id でタグ付けられた全エントリを
削除します。

**匿名 `mem://` 上の `Connection::publish`** は永遠に 0 を返します
(subscriber は存在不可)。`mem://<name>` 上では実際の receiver 数を
返します。

**TLS / AUTH** は非サポート。必要ならネットワーク境界で stunnel + IP
allowlist で前置きしてください。

## 非同期 runtime(tokio / async-std / smol)

`Subscription` と `Subscriber` は `Send + Sync` —— `Arc<Subscription>`
が動作するため、複数の async タスク(または `spawn_blocking` ジョブ)
が 1 つのハンドルを共有可能。ブロッキングの `recv` API は意図的に保持:
kevy は crates.io 依存ゼロで出荷するため、async-runtime-agnostic な
future は手書きが必要です。3 つのクリーン・パターン:

**Pattern A —— 専用 OS スレッド + runtime チャネル**(単一 consumer、
共有ハンドル不要):

```rust,no_run
# use kevy_embedded::{Config, PubsubFrame, Store};
# let store = Store::open(Config::default().with_ttl_reaper_manual())?;
// 擬似コード —— `runtime_channel` を tokio::sync::mpsc /
// async_channel / 等、runtime に応じて置換。
let (tx, rx) = /* runtime_channel */;
std::thread::spawn({
    let store = store.clone();
    move || {
        let sub = store.subscribe(&[b"queue:notify"]);
        while let Ok(frame) = sub.recv() {
            if matches!(
                frame,
                PubsubFrame::Message { .. } | PubsubFrame::Pmessage { .. }
            ) && tx.blocking_send(()).is_err()
            {
                break; // receiver dropped
            }
        }
    }
});
// `rx` が async 側ハンドル;async ループから await。
# Ok::<(), std::io::Error>(())
```

これは mailrs の outbound-queue worker が使うもの —— 小さい、長命の
タスク;recv 毎の tokio blocking-pool スロットを回避します。

**Pattern B —— `Arc<Subscription>` + `spawn_blocking`**(複数 async
タスクが 1 ハンドルを共有):

```rust,no_run
# use kevy_embedded::{Config, Store};
# use std::sync::Arc;
# let store = Store::open(Config::default().with_ttl_reaper_manual())?;
let sub = Arc::new(store.subscribe(&[b"queue:notify"]));
// 各 async タスクは Arc のクローンを取得し spawn_blocking 経由で
// recv;receiver mutex が並行 recv を直列化します。各フレームは
// ちょうど 1 タスクに配信(broadcast **ではない**)。
//
// ブロードキャスト扇出(全 consumer が全メッセージを見る)には、
// consumer 毎に別 Subscription を open —— 安価です。
let task_handle = {
    let sub = sub.clone();
    // tokio::task::spawn_blocking 擬似:
    std::thread::spawn(move || {
        loop {
            match sub.recv() {
                Ok(frame) => { /* 処理 */ let _ = frame; }
                Err(_) => break, // bus closed
            }
        }
    })
};
# let _ = task_handle;
# Ok::<(), std::io::Error>(())
```

`Subscription::try_recv` は `try_lock` を使い、ロック競合下では
`Ok(None)` を返します —— 別タスクが `recv` 経由で receiver を持って
いても non-blocking 契約は保たれます。

**Pattern C —— `kevy-client::Subscriber` の借用イテレータ**:

```rust,no_run
# use kevy_client::Subscriber;
let mut sub = Subscriber::open("mem://news", &[b"updates"])?;

// `events()` は全フレーム(ack 含む)を yield。UnexpectedEof で終了;
// 他のエラーは Some(Err(_)) として出るので、呼び出し側が retry
// (例:read timeout)か break かを判断。
for event in sub.events() {
    let _ = event?; // dispatch
    # break;
}

// `messages()` は ack を静かに消費し、`(channel, payload)` だけを
// yield —— recv_message が返す形と同じ。
let mut sub2 = Subscriber::open("mem://news", &[b"updates"])?;
for msg in sub2.messages() {
    let (_channel, _payload) = msg?;
    # break;
}
# Ok::<(), std::io::Error>(())
```

同じ `spawn_blocking` ルールが適用:イテレータは `recv` /
`recv_message` をラップし、各ブロッキング wait の期間 receiver
mutex を取ります。drop または break で wait を早期解放してください。
イテレータ API は `kevy-client` のもの、`kevy-embedded` のものではあ
りません —— 欲しい場合は URL facade に対して `Subscriber` を open し
てください;embed-only プリミティブが必要なら `Subscription` に直接
リーチしてください。

## 関連

- [`kevy-embedded` 1.2.0+](https://crates.io/crates/kevy-embedded) ——
  基盤の `Store::Clone` + `PubsubBus` プリミティブ。URL facade の間接
  層が不要なら直接使用してください。
- [`kevy-client` 1.9.0+](https://crates.io/crates/kevy-client) —— URL
  facade 自体。`Subscriber::recv_message`、`events()` / `messages()`
  イテレータ、slot ルーティングされた keyspace トラフィック用の
  `ClusterClient` を ship(pub/sub には不要 —— 上のクラスタ注記参照)。
- [`kevy`](https://crates.io/crates/kevy) —— TCP サーバー(1.17.0+)、
  単一プロセスを超えた場合に。
- [`docs/cluster.md`](cluster.md) —— クラスタ・モードと slot ルー
  ティングされた keyspace トラフィック用の `ClusterClient`。

# Pub/sub

kevy で 1 つの発行者から多数の購読者にメッセージをファンアウトする方法 — ワイヤ上では `PUBLISH` / `SUBSCRIBE`、プロセス内では組み込みの `Store` 経由、そして `kevy-client` の他部分と同じ URL ファサード越し — を説明します。

## このドキュメントが必要になるとき

1 つのライターがゼロまたはそれ以上のリーダーに*いま*通知したく、リーダーがオフラインの間に届いたメッセージは気にしない、という場面で pub/sub に手を伸ばします:

- 「全 Web ワーカーに config キャッシュをリフレッシュしろと伝える。」
- 「あるシャードから書き込まれたばかりの行を、tail している誰かにストリーミングする。」
- 「ジョブが着地したらワーカープールを起こす。ジョブ本体はリストに置く。」
- 「開発ループ: プロデューサスレッドとコンシューマスレッドが同じバイナリ内。Redis サーバー不要。」

耐久性のある hand-off(リトライ付きジョブキュー、再起動を超えるファンアウト、メッセージリプレイ)が必要なら、リストまたはストリームを使ってください — 何がディスクに書かれるかは [`docs/persistence.md`](persistence.md) を参照。

## 中心となる考え方

pub/sub のチャネルは名前です。購読者はその名前(またはグロブパターン)への関心を登録します。同じ名前への publish は購読者インデックスを歩き、マッチする購読者それぞれにボディのコピーを 1 つキューします。ブローカーキューも、オフラインバッファも、ack もありません — publish の瞬間に誰も聴いていなければ、メッセージは消えます。

```
                   publish("news", body)
                          |
                          v
             +-----------------------+
             |  channel "news"       |   <- チャネルごとの購読者インデックス
             |  subscribers: [A,B,C] |
             +-----------------------+
                  |       |       |
                  v       v       v
               sub A   sub B   sub C    <- それぞれが自分のコピーを受ける
```

内部では各 publish はワイヤフレームを 1 度だけ構築し、ボディを `Arc` で包み、`writev` でマッチする全 TCP 購読者に scatter-gather します — ファンアウトがどれだけ広くてもボディバイトは追加コピー**ゼロ**です。同じチャネル別インデックスがサーバー接続とプロセス内 `Subscription` ハンドルの両方を扱います。

## 動かしてみる例

### `redis-cli` でスモークテスト

動作中の kevy サーバーに対して 2 つのシェルを開きます:

```sh
# シェル 1 — 購読者
$ redis-cli -p 6379 SUBSCRIBE news
Reading messages... (press Ctrl-C to quit)
1) "subscribe"
2) "news"
3) (integer) 1
```

```sh
# シェル 2 — 発行者
$ redis-cli -p 6379 PUBLISH news "hello"
(integer) 1   # 1 人の購読者が受け取った
```

シェル 1 に戻ると:

```
1) "message"
2) "news"
3) "hello"
```

購読者ゼロのチャネルへの `PUBLISH` は `(integer) 0` を返し、メッセージは捨てられます。これが契約です — 「配信を試みた」シグナルは出ません。

### URL ファサード越しの Rust — `kevy-client`

同じ呼び出し形状で TCP サーバー、名前付きプロセス内バス、永続的なプロセス内ストアを狙えます。URL を切り替えて再コンパイルするだけで、呼び出し側に `match scheme { … }` はいりません。

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

fn run(url: &str) -> std::io::Result<()> {
    // `news` に対する購読者を開く。バスが最初に返すフレームは subscribe ack なので、
    // ボディをアサートする前にドレインする。
    let mut sub = Subscriber::open(url, &[b"news"])?;
    let _ack = sub.recv()?;

    let mut conn = Connection::open(url)?;
    let received = conn.publish(b"news", b"hello")?;
    assert_eq!(received, 1);

    match sub.recv()? {
        PubsubEvent::Message { channel, payload } => {
            assert_eq!(channel, b"news");
            assert_eq!(payload, b"hello");
        }
        other => panic!("unexpected frame: {other:?}"),
    }
    Ok(())
}

// 開発:  名前付きのプロセス内共有バス。
run("mem://app")?;
// 本番: 実際の TCP サーバー。
run("kevy://prod-cache:6379")?;
# Ok::<(), std::io::Error>(())
```

クロススレッドは同じコードで、別スレッドから同じ URL に対して `Subscriber` 1 つと `Connection` 1 つを開くだけです — `mem://<name>` レジストリが両端に同じバッキングバスを渡すので、プロデューサスレッドが `Connection::publish` し、コンシューマスレッドが `sub.recv()` でブロックします。

### `kevy-embedded` 経由のプロセス内

組み込みコードが既に `Store` を持っているなら、URL 経由を飛ばして直接バスと話します:

```rust
use kevy_embedded::{Config, PubsubFrame, Store};

let store = Store::open(Config::default().with_ttl_reaper_manual())?;

// 購読者は受信キューを所有する。
let sub = store.subscribe(&[b"jobs"]);
let _ack = sub.recv()?; // PubsubFrame::Subscribe

// `store` のどのクローンも同じバスに届く。
let writer = store.clone();
assert_eq!(writer.publish(b"jobs", b"compute-pi"), 1);

match sub.recv()? {
    PubsubFrame::Message { channel, payload } => {
        assert_eq!(channel, b"jobs");
        assert_eq!(payload, b"compute-pi");
    }
    other => panic!("unexpected frame: {other:?}"),
}
# Ok::<(), std::io::Error>(())
```

`Store::clone` は安い(`Arc` のバンプ)ので、典型形は「各スレッドに `store.clone()` を渡し、必要なときに `publish` か `subscribe` をさせる」です。購読者の drop はアトミックに登録解除されます。コンシューマスレッドがパニックしてもインデックスにゾンビエントリは残りません。

### パターン購読

`PSUBSCRIBE` はグロブを登録し、それにマッチするどのチャネルのメッセージも受けます。グロブ構文 — `*`、`?`、`[abc]` — は `KEYS` と `SCAN` が使うマッチャと同じです。

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

let mut sub = Subscriber::connect("mem://signals")?;
sub.psubscribe(&[b"news.*"])?;
let _ack = sub.recv()?;            // PubsubEvent::Psubscribe

let mut conn = Connection::open("mem://signals")?;
conn.publish(b"news.tech", b"breaking")?; // マッチ
conn.publish(b"weather",   b"sunny")?;    // マッチしない

match sub.recv()? {
    PubsubEvent::Pmessage { pattern, channel, payload } => {
        assert_eq!(pattern, b"news.*");
        assert_eq!(channel, b"news.tech");
        assert_eq!(payload, b"breaking");
    }
    other => panic!("unexpected frame: {other:?}"),
}
# Ok::<(), std::io::Error>(())
```

チャネル購読と**かつ**マッチするパターン購読の両方を持つ購読者は**2 つ**のコピーを受けます — `Message` 1 つと `Pmessage` 1 つ。発行ごとの dedup は「同じ `Subscription` が同じチャネルインデックスに 2 回並んでいる」重複だけを抑止し、チャネル vs パターンの重なりは抑止しません。

## URL バックエンド表

| URL                                | バッキングストア              | 開くたびに共有?                              | プロセスを跨いで可視? |
|------------------------------------|----------------------------|---------------------------------------------------|-----------------------|
| `mem://`                           | プロセス内、匿名      | **いいえ** — 開くたびに新しい `Store`           | いいえ                    |
| `mem://<name>`                     | プロセス内、名前付きレジストリ | **はい** — 同じ `<name>` ⇒ 同じ `Store`            | いいえ                    |
| `file:///abs/path`                 | プロセス内 + AOF/snapshot  | **はい** — 同じ path ⇒ 同じ `Store`、永続      | いいえ                    |
| `kevy://host[:port][/db]`          | TCP の kevy サーバー            | 開くごとに 1 ソケット、サーバー側でファンアウト         | **はい**               |
| `redis://host[:port][/db]`         | TCP — `kevy://` のエイリアス   | 同じ                                              | **はい**               |
| `tcp://host[:port]`                | TCP — 生、`SELECT` 先導なし | 同じ                                          | **はい**               |

匿名 `mem://` は発行されたメッセージを受け取れません — 同じバッキング `Store` に他のものは届かないので、`Subscriber::open` は `ErrorKind::Unsupported` で拒否します。発行する意図があるときは常に `mem://<some-name>` を使ってください。

`rediss://`、`kevys://`、`redis://user:pass@…` は同じ理由で拒否されます: kevy は TLS や `AUTH` なしで出荷されます。どちらかが必要ならネットワーク境界で stunnel + IP allowlist を被せてください。

`mem://<name>` と `file:///` のレジストリは**プロセス単位**です: 同じ名前を開いた無関係な 2 つの OS プロセスは独立した 2 つのバスを見ます。プロセスを跨いだ配信が欲しいなら、kevy サーバーを動かして両側から `kevy://host:port` を開いてください。

## トレードオフと限界

- **At-most-once 配信。** フレーム途中で切断した購読者はそのフレームを失います。購読者ごとの耐久性カーソルも再配信もありません。フレームが重要なら、リストかストリームで永続化し、pub/sub は「起こす」シグナルとしてだけ使ってください。
- **オフラインバックログなし。** 購読者ゼロを見つけた publish は `0` を返してボディを破棄します。切断中に見逃したものを購読者に追いつかせるバッファはありません。
- **購読者のバックプレッシャは購読者単位で、グローバルではありません。** 各購読者は自分の有界キューを所有します。遅いコンシューマは自分のキューを埋め、それからフレームを落とすか、TCP ならサーバーのクライアント出力バッファポリシーで閉じられます。publish パスは送信前にバスのミューテックスを離すので、遅いリスナー 1 人が無関係なチャネルの publish を止めることはできません — が、発行者へバックプレッシャを掛けることもできません。
- **Linux `writev` の上限。** Linux 上、`writev` は呼び出しごとに最大 `IOV_MAX = 1024` の iovec エントリしかカーネルに渡せません。サーバーは購読者ごとのフレームヘッダと共有ボディの Arc を iovec にまとめます。チャネルあたり約 340 を超える購読者(各 iovec 3 つ)へのファンアウトでは、サーバーは複数の `writev` 呼び出しに自動分割します。上限はソフトな性能の天井としてしか出ず、配信失敗にはなりません。
- **subscribed クライアントは制限されます。** `Subscriber` コネクションは pub/sub 以外のコマンドを拒否します。だから `kevy-client` は発行者と購読者を**別の 2 つの型**として、同じ URL を共有させて公開します。

## 運用イントロスペクション

標準の `PUBSUB` 管理サブコマンドは TCP サーバーでも URL ファサードでも動きます — 呼び出すには `Subscriber` ではなく通常の `Connection` を開きます。

| サブコマンド              | 戻り値                                                                        |
|-------------------------|--------------------------------------------------------------------------------|
| `PUBSUB CHANNELS [pat]` | 少なくとも 1 人の購読者がいるチャネルの配列。オプションでグロブフィルタ。      |
| `PUBSUB NUMSUB [ch …]`  | 名前付きチャネルごとに `channel, count` ペアをインターリーブ(なければ 0)。       |
| `PUBSUB NUMPAT`         | 整数: 全クライアントを通じて登録された `PSUBSCRIBE` パターンの異なり数。  |

```sh
$ redis-cli -p 6379 PUBSUB CHANNELS '*'
1) "news"
2) "jobs"
$ redis-cli -p 6379 PUBSUB NUMSUB news jobs missing
1) "news"
2) (integer) 3
3) "jobs"
4) (integer) 1
5) "missing"
6) (integer) 0
$ redis-cli -p 6379 PUBSUB NUMPAT
(integer) 2
```

3 つともシャードごとの pub/sub レジストリに対する O(channels) または O(args) のポイントルックアップで、監視エージェントからのポーリングは安全です。

## FAQ

**publish の後で接続した購読者にメッセージは届きますか?**  いいえ。pub/sub にリプレイはありません。購読者インデックスは publish 時点で参照されます。後から購読した者は、自分の subscribe ack が着地した*後*に発行されたフレームしか見ません。

**`PUBLISH` は購読者がドレインするまで発行者をブロックしますか?**  いいえ。発行者の `publish` 呼び出しは、ボディがマッチする全購読者の購読者別キューにキューされ次第(TCP 購読者なら加えてそれぞれのソケットの書き込みキューにスケジュールされ次第)戻ります。遅い購読者は自分のキューを止めるだけで、あなたのを止めません。

**1 つの `Subscriber` を async タスク間で共有できますか?**  はい — `Arc` で包んで `recv` 呼び出しを `spawn_blocking` してください。受信ミューテックスがブロッキング待機を直列化するので、各フレームは**ちょうど 1 つ**のタスクに配信されます。本当のブロードキャストファンアウト(全タスクが全フレームを見る)が欲しければ、タスクごとに 1 つの `Subscriber` を開いてください — 安いです。完全な async パターンは [`docs/async.md`](async.md) を参照。

**なぜテストはメッセージより前に subscribe ack を見ますか?**  バスは順序付きですが、各 `SUBSCRIBE` / `PSUBSCRIBE` は、そのチャネルの最初のボディフレームより*先に* ack フレームをキューします。ペイロードをアサートする前に `sub.recv()?` 1 回で ack をドレインしてください — これは redis-cli のワイヤ形状と一致します。

**pub/sub にクラスタルーティングは必要ですか?**  いいえ。Pub/sub ファンアウトはプロセスレベルでスロットルーティングではありません: 任意のシャードのポートで publish すれば、同じプロセス内の任意のシャードのポートの全購読者に届きます。任意のシャードポートに対する `Connection::open("kevy://host:port")` で十分です。*キー空間*コマンドが使うスロットルーティングについては [`docs/cluster.md`](cluster.md) を参照。

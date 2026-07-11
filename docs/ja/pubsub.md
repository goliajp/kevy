# Pub/sub

kevyで1つの発行者から多数の購読者へメッセージをファンアウトする方法を説明します。ワイヤ上では`PUBLISH`/`SUBSCRIBE`で、プロセス内では組み込みの`Store`経由で、そして`kevy-client`のほかの部分と同じURLファサード越しに使えます。

## このドキュメントが必要になるとき

1つのライターが0以上のリーダーに*いますぐ*通知したい、そしてリーダーがオフラインの間に届いたメッセージは気にしない。そういう場面がpub/subの出番です：

- 「全Webワーカーにconfigキャッシュをリフレッシュしろと伝える。」
- 「あるシャードに書き込まれたばかりの行を、tailしている誰かにストリーミングする。」
- 「ジョブが着地したらワーカープールを起こす。ジョブ本体はリストに置く。」
- 「開発ループ：プロデューサスレッドとコンシューマスレッドが同じバイナリ内。Redisサーバーは不要。」

耐久性のあるハンドオフ（リトライ付きジョブキュー、再起動をまたぐファンアウト、メッセージリプレイ）が必要なら、代わりにリストかストリームを使ってください。何がディスクに書かれるかは[`docs/persistence.md`](persistence.md)を参照してください。

## 中心となる考え方

pub/subのチャネルとは名前のことです。購読者はその名前（またはグロブパターン）への関心を登録し、同じ名前へのpublishは購読者インデックスを歩いて、マッチした購読者ごとにボディのコピーを1つキューに積みます。ブローカーキューも、オフラインバッファも、ackもありません。publishした瞬間に誰も聴いていなければ、そのメッセージは消えます。

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

内部では、各publishはワイヤフレームを1度だけ構築し、ボディを`Arc`で包み、`writev`でマッチする全TCP購読者にscatter-gatherします。つまりファンアウトがどれだけ広くても、ボディのバイト列の追加コピーは**ゼロ**です。同じチャネル別インデックスが、サーバー接続とプロセス内の`Subscription`ハンドルの両方を扱います。

## 動かしてみる例

### `redis-cli`でスモークテスト

動作中のkevyサーバーに対してシェルを2つ開きます：

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

シェル1に戻ると：

```
1) "message"
2) "news"
3) "hello"
```

購読者のいないチャネルへの`PUBLISH`は`(integer) 0`を返し、メッセージは捨てられます。これが契約です。「配信を試みた」という類のシグナルは出ません。

### URLファサード越しのRust — `kevy-client`

同じ呼び出しの形で、TCPサーバー、名前付きプロセス内バス、永続的なプロセス内ストアのどれでも狙えます。URLを切り替えて再コンパイルするだけで、呼び出し側に`match scheme { … }`は要りません。

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

fn run(url: &str) -> kevy_client::KevyResult<()> {
    // `news` に対する購読者を開く。バスが最初に返すフレームは subscribe ack なので、
    // ボディをアサートする前にドレインする。
    let mut sub = Subscriber::connect_channels(url, &[b"news"])?;
    let _ack = sub.recv()?;

    let mut conn = Connection::connect(url)?;
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
# Ok::<(), kevy_client::KevyError>(())
```

スレッドをまたぐ場合もコードは同じです。別々のスレッドから同じURLに対して`Subscriber`と`Connection`を1つずつ開くだけです。`mem://<name>`のレジストリが両端に同じバッキングバスを渡すので、プロデューサスレッドが`Connection::publish`し、コンシューマスレッドが`sub.recv()`でブロックします。

### `kevy-embedded`経由のプロセス内利用

組み込み側のコードがすでに`Store`を持っているなら、URLの間接層を飛ばして直接バスと話せます：

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
# Ok::<(), kevy_client::KevyError>(())
```

`Store::clone`は安価（`Arc`のカウントを増やすだけ）なので、典型的な形は「各スレッドに`store.clone()`を渡し、必要になったときに`publish`や`subscribe`をさせる」というものです。購読者のdropはアトミックに登録解除されるため、コンシューマスレッドがパニックしてもインデックスにゾンビエントリは残りません。

### パターン購読

`PSUBSCRIBE`はグロブを登録し、それにマッチするすべてのチャネルのメッセージを受け取ります。グロブ構文（`*`、`?`、`[abc]`）は、`KEYS`と`SCAN`が使うマッチャと同じです。

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

let mut sub = Subscriber::connect("mem://signals")?;
sub.psubscribe(&[b"news.*"])?;
let _ack = sub.recv()?;            // PubsubEvent::Psubscribe

let mut conn = Connection::connect("mem://signals")?;
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
# Ok::<(), kevy_client::KevyError>(())
```

チャネル購読と、それにマッチするパターン購読の**両方**を持つ購読者は、コピーを**2つ**受け取ります（`Message`が1つと`Pmessage`が1つ）。publishごとの重複排除が抑止するのは「同じ`Subscription`が同じチャネルインデックスに2回並んでいる」タイプの重複だけで、チャネルとパターンの重なりは抑止しません。

## URLバックエンド表

| URL                                | バッキングストア              | openをまたいで共有される？                              | プロセスをまたいで見える？ |
|------------------------------------|----------------------------|---------------------------------------------------|-----------------------|
| `mem://`                           | プロセス内、匿名      | **いいえ** — openのたびに新しい`Store`           | いいえ                    |
| `mem://<name>`                     | プロセス内、名前付きレジストリ | **はい** — 同じ`<name>` ⇒ 同じ`Store`            | いいえ                    |
| `file:///abs/path`                 | プロセス内 + AOF/スナップショット  | **はい** — 同じパス ⇒ 同じ`Store`、永続      | いいえ                    |
| `kevy://host[:port][/db]`          | TCPのkevyサーバー            | openごとに1ソケット、サーバー側でファンアウト         | **はい**               |
| `redis://host[:port][/db]`         | TCP — `kevy://`のエイリアス   | 同上                                              | **はい**               |
| `tcp://host[:port]`                | TCP — 生。先頭の`SELECT`なし | 同上                                          | **はい**               |

匿名の`mem://`は発行されたメッセージを受け取れません。同じバッキング`Store`にほかの誰も到達できないため、`Subscriber::connect_channels`は`ErrorKind::Unsupported`で拒否します。publishするつもりがあるなら、常に`mem://<some-name>`を使ってください。

`rediss://`、`kevys://`、`redis://user:pass@…`も同じ理由で拒否されます。kevyはTLSも`AUTH`もなしで出荷されるからです。どちらかが必要なら、ネットワーク境界でstunnelとIP許可リストを前段に置いてください。

`mem://<name>`と`file:///`のレジストリは**プロセス単位**です。無関係な2つのOSプロセスが同じ名前を開いても、見えるのは独立した2つのバスです。プロセスをまたいだ配信が欲しいなら、kevyサーバーを立てて両側から`kevy://host:port`を開いてください。

## トレードオフと限界

- **at-most-once配信。** フレームの途中で切断した購読者は、そのフレームを失います。購読者ごとの耐久カーソルも再配信もありません。フレームが重要なら、リストかストリームに永続化し、pub/subは「起こす」シグナルとしてだけ使ってください。
- **オフラインバックログなし。** 購読者ゼロのpublishは`0`を返してボディを破棄します。切断中に見逃した分を購読者に追いつかせるバッファはありません。
- **購読者のバックプレッシャは購読者単位で、グローバルではありません。** 各購読者は自分専用の有界キューを持ちます。遅いコンシューマは自分のキューを埋め、その後はフレームを落とすか、TCPならサーバーのクライアント出力バッファポリシーによって切断されます。publishパスは送信前にバスのミューテックスを手放すので、遅いリスナー1人が無関係なチャネルのpublishを止めることはできません。その代わり、発行者へバックプレッシャを掛けることもできません。
- **Linuxの`writev`上限。** Linuxでは、`writev`が1回の呼び出しでカーネルに渡せるiovecエントリは最大`IOV_MAX = 1024`です。サーバーは購読者ごとのフレームヘッダと共有ボディのArcをiovecにまとめます。1購読者あたりiovecを3つ使うため、チャネルあたり約340購読者を超えるファンアウトでは、サーバーが自動的に複数回の`writev`呼び出しに分割します。この上限はソフトな性能の天井として現れるだけで、配信失敗にはなりません。
- **購読中のクライアントは制限されます。** `Subscriber`コネクションはpub/sub以外のコマンドを拒否します。`kevy-client`が発行者と購読者を、同じURLを共有する**別々の2つの型**として公開しているのはこのためです。

## 運用イントロスペクション

標準の`PUBSUB`管理サブコマンドはTCPサーバーでもURLファサードでも動きます。呼び出すときは`Subscriber`ではなく通常の`Connection`を開いてください。

| サブコマンド              | 戻り値                                                                        |
|-------------------------|--------------------------------------------------------------------------------|
| `PUBSUB CHANNELS [pat]` | 購読者が1人以上いるチャネルの配列。オプションでグロブフィルタ可。      |
| `PUBSUB NUMSUB [ch …]`  | 指定した各チャネルについて`channel, count`のペアを交互に返す（存在しなければ0）。       |
| `PUBSUB NUMPAT`         | 整数。全クライアントを通じて登録されている`PSUBSCRIBE`パターンの異なり数。  |

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

3つともシャードごとのpub/subレジストリに対する`O(channels)`または`O(args)`のポイントルックアップなので、監視エージェントからポーリングしても安全です。

## FAQ

**publishの後で接続した購読者にメッセージは届きますか？**  いいえ。pub/subにリプレイはありません。購読者インデックスはpublish時点で参照されます。後から購読した者に見えるのは、自分のsubscribe ackが着地した*後*に発行されたフレームだけです。

**`PUBLISH`は購読者がドレインするまで発行者をブロックしますか？**  いいえ。発行者の`publish`呼び出しは、ボディがマッチする全購読者の購読者別キューに積まれた時点（TCP購読者の場合はさらに各ソケットの書き込みキューにスケジュールされた時点）で戻ります。遅い購読者が詰まらせるのは自分のキューであって、あなたのキューではありません。

**1つの`Subscriber`をasyncタスク間で共有できますか？**  はい。`Arc`で包んで`recv`呼び出しを`spawn_blocking`してください。受信ミューテックスがブロッキング待機を直列化するので、各フレームは**ちょうど1つ**のタスクに配信されます。本当のブロードキャストファンアウト（全タスクが全フレームを見る）が欲しければ、タスクごとに`Subscriber`を1つ開いてください。開くのは安価です。完全なasyncパターンは[`docs/async.md`](async.md)を参照してください。

**なぜテストではメッセージより先にsubscribe ackが見えるのですか？**  バスは順序付きですが、各`SUBSCRIBE`/`PSUBSCRIBE`は、そのチャネルの最初のボディフレームより*先に*ackフレームをキューに積みます。ペイロードをアサートする前に、`sub.recv()?`を1回呼んでackをドレインしてください。これはredis-cliのワイヤ上の挙動とも一致します。

**pub/subにクラスタルーティングは必要ですか？**  いいえ。pub/subのファンアウトはプロセスレベルで、スロットルーティングされません。どのシャードのポートでpublishしても、同じプロセス内の全シャードのポートの全購読者に届きます。任意のシャードポートへの`Connection::connect("kevy://host:port")`で十分です。*キー空間*コマンドが使うスロットルーティングについては[`docs/cluster.md`](cluster.md)を参照してください。

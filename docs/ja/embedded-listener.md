# 組み込みの読み取り専用RESPリスナー

組み込み（プロセス内）のkevyストアは、読み取り専用のリスナーを通じて、外部のRESPクライアント——redis-cli、運用ツール、ダッシュボード——に自分を公開できます。ストアはあなたのプロセスの中のライブラリのままです。リスナーはそこを覗く窓であって、2台目のサーバーではありません。書き込みは、所有プロセスの排他的な権利であり続けます。

```rust
use kevy_embedded::{Config, Store};

fn main() -> kevy_embedded::KevyResult<()> {
    let store = Store::open(
        Config::default()
            .with_shards(4)
            .with_resp_listener("127.0.0.1:6009".parse().unwrap()),
    )?;
    store.hset(b"row:42", &[
        (b"state".as_slice(), b"live".as_slice()),
    ])?;
    // ... the application keeps running; clients can peek:
    Ok(())
}
```

```
$ redis-cli -p 6009 hgetall row:42
1) "state"
2) "live"
$ redis-cli -p 6009 scan 0 match 'row:*' count 100
$ kevy-cli -p 6009 DBSIZE
```

RESPクライアントならどれでも動きます。リスナーはkevyサーバーと同じプロトコルを話し、フレーム化されたリクエストもインラインコマンド（`redis-cli`のPING形式）も受け付けます。

## 有効にする

- `Config::with_resp_listener(addr)` — `SocketAddr`を1つ渡します。**デフォルトはオフ**です。オフのときは**スレッドもソケットも存在しません——税はゼロ**です（ゲート済み：リスナーを有効にしたまま遊休させたときの書き込みスループットが、オフのときの10%以内。[`bench/topogate.sh`](../../bench/topogate.sh)）。
- コードは`listener` cargoフィーチャの背後にあります（デフォルトで有効。wasm32ターゲットでは利用できません）。
- リスナーが持つのは弱いハンドルだけです。ストアを生かし続けることはなく、ストアがdropされればコネクションも終わります。
- 認証はありません（意図的です。kevyにAUTHプレーンは存在しません）。ループバックかプライベートインタフェースにbindしてください。到達できる者は誰でも、ホワイトリストが提供するものすべてを読めます。

## 面

ホワイトリストのみです。それ以外はすべて`-ERR READONLY embedded listener`を返します。

```
PING ECHO GET MGET EXISTS TYPE TTL PTTL DBSIZE KEYS SCAN
HGET HMGET HGETALL HLEN LRANGE LLEN SMEMBERS SCARD SISMEMBER
ZSCORE ZCARD ZRANGE FEED.READ FEED.TAIL FEED.SHARDS INFO
```

拒否される側には、すべての書き込みverb、`MULTI`、ブロッキングpop、pub/sub、そして拡張プレーン（`IDX.*` / `VIEW.*`——これらはプロセス内から型付きAPIでクエリしてください）が含まれます。`FEED.*`の3つは`replicate` cargoフィーチャを必要とします（デフォルトで有効）。

`INFO`は、組み込み向けの小さなレポートを返します。crateのバージョン、シャード数、キー数、そして`listener:readonly`——ダッシュボードが「自分は何と話しているのか」を識別するには十分な内容です。

この拒否テキストは契約です。ツールは書き込みを試み、`READONLY embedded listener`にマッチするかどうかで「これは本物のサーバーか、組み込みの窓か」を判定できます。

### verbのセマンティクス

ホワイトリストのverbは、サーバー側の同名verbと同じように振る舞います。

- `SCAN <cursor> [MATCH pattern] [COUNT n]`は数値カーソルでページングし（`COUNT`は1..=10000にclampされ、デフォルトは100）、通常のSCAN保証を持ちます。走査全体を通じて安定していたキーはちょうど1回見えます。並行する挿入・削除は現れるかもしれないし、現れないかもしれません。
- `KEYS <pattern>`はキー空間全体を1回の応答で走査します（`*`はすべてにマッチ）——スポットチェック専用です。大きなストアでは`SCAN`を使ってください。
- 型の不一致は、サーバーとまったく同じように`-WRONGTYPE …`を返します。壊れた整数は`-ERR value is not an integer`を返します。

## 整合性

読み出しはストア自身のシャードロックのもとで走ります。どの応答も、コミットされた、ある時点の答えです。書き込みプロセスと同時刻であり（レプリケーションなし、ラグなし、スナップショットの古さなし）。マルチキーの読み出し（`MGET`、`SCAN`、`KEYS`、`DBSIZE`）は、グローバルスナップショットなしにシャードごとにマージされます——サーバーと同じ、SCANクラスの包絡線です。

コネクションごとに1スレッドです。これは運用ツール向けの面であって、サービングパスではありません（kevy*サーバー*こそがサービングパスです）。コネクション数はツールの規模にとどめてください。数秒おきにポーリングするダッシュボード、というのが想定している形です。

## フィードとread-your-writes

`FEED.TAIL`は現在の`(generation, offset)`を返します。`FEED.READ <gen> <offset> <limit> [PREFIX p…]`は変更フレームを配送します——組み込みの`changes_since` APIと同じat-least-onceの契約です（generationが古ければ`Resync`が返るので、`FEED.TAIL`からやり直してください）。組み込みの書き込みパスは全シャードを1本のストリームに直列化するため、`FEED.SHARDS`は1を返し、フィードのverbはシャード引数を取りません——サーバー面に対して書いたコンシューマループ（[cdc.md](cdc.md)）が、そのままここでも動きます。

プロセスをまたぐフィード越しのread-your-writesは、ブロッキングのプリミティブではなく、カーソルのパターンです。書き込むプロセスが、自分の書き込みのあとに`changes_tail()`を控えておきます。読むプロセスは、まず`FEED.READ`をそのカーソルの先までdrainし、それから読みます。プロセス内の読み出しは常にread-your-writesです（書き込みは同期的にコミットするからです）。（サーバーからレプリカへのレプリケーションには、ブロッキングのプリミティブが*実際にあります*——`REPL.TOKEN` / `REPL.WAIT`、[availability.md](availability.md)を参照——が、それはレプリケーションのプレーンであって、このフィードリスナーではありません。）

## ソケットなしで覗く

リスナーのverbテーブルは、public methodでもあります。

```rust
let mut out = Vec::new();
store.dispatch_readonly(
    &[b"HGETALL".to_vec(), b"row:42".to_vec()], &mut out);
// `out` holds raw RESP bytes — the exact reply the listener
// would have written to a socket.
```

`Store::dispatch_readonly(argv, out)`は、同じホワイトリストに対して1件のリクエストに答えます（書き込みverbには同じ`-ERR`が返ります）——リスナーのプログラム的な顔であり、所有プロセスに埋め込まれたツール向けです。

そして、**自分が所有していない**ストアに対しては、`kevy-cli --embed <dir>`があります。組み込みストアのデータディレクトリの、読み取り専用・ある時点のビューを開きます。dump/aof/shards.metaの各ファイルはスクラッチディレクトリへ**コピー**されてからリプレイされるので、所有プロセスは触れられることなく走り続けます。REPLも単発実行も動きます。書き込みverbにはリスナーと同じ`-ERR READONLY`が返ります。これは、このリスナーの生きた窓に対する、オフラインの補完物です。

## 窓より多くを求めるとき

機構の重さの順に、エスカレーションの道筋を示します。

1. **このリスナー** — 生きた読み出し、書き込みコストゼロ、1プロセス。
2. **CDCフィード**（[cdc.md](cdc.md)） — 相手プロセスに読み出しをポーリングさせるのではなく、変更をそちらへpushします。
3. **組み込みをプライマリにするレプリケーション** — `with_embed_writer`がレプリケーションソースを公開し、`[replication] single_source = true`のkevyサーバーが、その組み込みストアをレプリカとして追随します。あなたのプロセスが供給し、サーバーのハードウェア上で読み出しをフルにファンアウトする形です。[replication.md](replication.md)の*組み込みをプライマリにする*節を参照してください。

## パフォーマンス

[`bench/topogate.sh`](../../bench/topogate.sh)がclampであり、これは**本物の2プロセステスト**です。連続的なHSET負荷の下にあるライターのバイナリと、生きたデータを表明する別プロセスのリーダーからなります。

- 所有者が連続的に書き込んでいるあいだ、リーダーの`GET` p99 < 1ms（6コネクションの中央値）。
- READONLYの強制（書き込みverbが契約テキストとともに拒否されること）。
- ゼロ税。リスナーを有効にしたまま遊休させたときの所有者の書き込みスループットが、リスナーオフのときの10%以内。

読み出しがシャードロックのもとで走るという設計は、リスナーのトラフィックが重ければ、同じシャード上で所有者の書き込みと競合し**得る**ことを意味します。それが、ラグゼロの真実と引き換えに払う代償です。ツール規模の読み出しレート（ダッシュボード、スポットチェック）は、所有者の書き込みスループットの中では計測不能なほど小さくなります。サービング規模の読み出しが必要なら、このリスナーではなく、レプリケーション（上記）を使ってください。

## 関連項目

- [cdc.md](cdc.md) — このリスナーも提供する、変更フィード。
- [replication.md](replication.md) — 組み込みをプライマリにする。窓では足りなくなったときに。
- [availability.md](availability.md) — レプリケーションのプレーン上の整合性ラダー。
- [uds.md](uds.md) — kevyサーバー向けのローカルソケットトランスポート（リスナー自体はTCP専用です）。

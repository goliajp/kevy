# 暗号化リンク

レプリケーション、選挙のコントロールプレーン、クライアント接続は、それぞれ kevy 自身の暗号化され認証されたリンクの上で動かせます。どれも**明示的に有効にしない限りオフ**です。TLS と AUTH は今もありません。TLS が必要なクライアントは [deploy-behind-a-proxy.md](deploy-behind-a-proxy.md) のとおりプロキシ経由で接続し、暗号化ポートを開いても開かなくても平文のクライアントポートの振る舞いは変わりません。

## 対象となるリンク

| リンク | 暗号化される条件 | 認証される相手 |
|---|---|---|
| 選挙（`elect_port_base`） | `[cluster] secure = true` | 両端。`peer_keys` の鍵による |
| レプリケーション（`listen_port_base + i`） | `[replication] secure = true` | プライマリは `upstream_key` かピアの鍵で、レプリカは `replica_keys` を設定したときに認証される |
| `[secure] listen_port` のクライアント | 常に暗号化 | サーバーは自分の鍵で、`client_keys` を設定したときはクライアントも認証される |
| `port`、クラスタポート、unix ソケットのクライアント | 暗号化しない | 認証しない。プロキシを使う |

プロトコルは Noise `IK` で、X25519、ChaCha20-Poly1305、BLAKE2s を使います。イニシエータは接続前に相手の公開鍵を知っており、レスポンダは最初のメッセージからイニシエータの公開鍵を知り、応答する前に拒否できます。この 1 往復のあとの通信はすべて暗号化され、認証されます。暗号プリミティブは kevy 自身の実装で依存はなく、公開されたテストベクタや他の実装と照合済みですが、第三者の監査は受けていません。

## 鍵

各ノードに鍵ペアが必要です。`kevy keygen` は秘密鍵を書き出し、公開鍵を表示します。

```console
$ kevy keygen /etc/kevy/node.key
ffe2a40f453275a19e8b115332459968e9a3b54b9c2e728ec538ccdd724b2f6c
```

ファイルはモード `0600` で作られ、既存のファイルは上書きしません。他のユーザーが読める鍵ファイルでは、kevy は起動を拒否します。

## 設定

各ノードは自分の鍵を指定し、すべてのピアの公開鍵を列挙します。公開鍵は、各ノードで `kevy keygen` が表示したものです。次は `n2` の設定です。

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
replica_keys = []   # プライマリ側：接続を許すレプリカの鍵。空ならすべて許可
```

- `peers` のうち自分以外の各ノードについて、`peer_keys` に項目が必要です。自分の項目は無視されるので、全ノードで同じリストを共有できます。
- レプリカは `upstream_key` と `peer_keys` のすべての鍵をプライマリとして信頼します。選挙のあと、新しいプライマリの鍵はすでにリストにあるので、設定を変えずに追従します。
- プライマリの `replica_keys` は接続できるレプリカを制限します。空のままならどのレプリカも接続でき、リンクはそれでも暗号化されます。

kevy は起動時にこれを確認します。`secure = true` なのに `private_key_file` がない、鍵のないピアがある、`upstream_key` のない暗号化レプリカがある、のいずれでも、足りないものを示してノードは起動を止めます。平文に戻ることはありません。

## 組み込みストア

組み込みのライターとそのレプリカも同じ方法でリンクを暗号化します。設定は `kevy.toml` ではなくコードで行います。ハンドシェイクはサーバーと同じなので、組み込みレプリカは暗号化された kevy サーバーに追従でき、サーバーのレプリカも暗号化された組み込みライターに追従できます。

```rust
use kevy_embedded::{Config, Keypair, LinkKeys, Store};

# fn keys() -> (Keypair, Keypair) { (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32])) }
let (writer_key, replica_key) = keys();
let writer_public = writer_key.public();

// ライター：どのレプリカも接続でき、リンクは暗号化される
let writer = Store::open(
    Config::default()
        .with_embed_writer("0.0.0.0:7101")
        .with_writer_security(LinkKeys::new(writer_key)),
)?;

// レプリカ：ライターの公開鍵を信頼する
let replica = Store::open(
    Config::default()
        .without_aof()
        .with_replica_upstream("writer.internal:7101")
        .with_replica_security(LinkKeys::new(replica_key).with_peers(vec![writer_public])),
)?;
# Ok::<(), kevy_embedded::KevyError>(())
```

- レプリカでは、`peers` は信頼するプライマリの一覧で、応答があるまで順に試します。空だと開くときに失敗します。ライターでは、接続を許すレプリカの一覧で、空ならすべて許可します。
- `Keypair::from_secret` が受け取る 32 バイトは、`kevy keygen` が hex で書き出すものです。どこに保管するかはアプリケーションが決めます。

## クライアント

`[secure] listen_port` は暗号化プロトコルだけを話す 2 つ目のクライアントポートを開きます。平文の `port` はそのままです。クライアントはサーバーの公開鍵を含む `kevys://` URL で接続します。

```toml
[secure]
private_key_file = "/etc/kevy/node.key"
listen_port      = 6404
client_keys      = []   # 許可するクライアントの公開鍵。空ならすべて許可し、それでも暗号化される
```

```text
kevys://10.0.0.11:6404?server_key=d5c015b88401b6b33f5cb292b01ff3034e5f1de008b2e3f77faed23c31f16a4c
kevys://10.0.0.11:6404/0?server_key=<hex>&client_key_file=/etc/app/kevy.key
```

- `client_key_file` のないクライアントは、接続ごとに新しい鍵ペアを作ります。`client_keys` が空ならそれで足ります。空でないときは `kevy keygen` でクライアントの鍵を作り、その公開鍵を列挙してください。
- `kevys://` を受け付けるのは Rust クライアントです。`kevy-resp-client`（`RespClient::connect_url`、または任意の RESP コードの下に `SecureStream` を置く）、`kevy-client`（`Connection` と `Subscriber`）、`kevy-client-async`（`AsyncConnection::connect_secure_url`、`AsyncSubscriber::connect_secure_url`）です。`kevy-cli -u` も受け付けます。ほかの言語のバインディングは対応していないので、TLS プロキシを使ってください。
- `CLIENT LIST`、`CLIENT INFO`、`CLIENT KILL ADDR` にはサーバーではなくクライアント自身のアドレスが表示されます。
- このポートには `private_key_file` が必要で、`port` と同じにはできません。どちらの設定ミスでも、サーバーは起動時に停止します。

## クラスタモード

クラスタモードでは、各シャードのクラスタポートに暗号化された双子のポートが付き、そこから平文のクラスタポートへ中継されます。暗号化クライアントも平文クライアントと同じくスロットでルーティングします。

```toml
[cluster]
enabled = true              # シャード i は port_base + i（既定は port + 1）

[secure]
private_key_file  = "/etc/kevy/node.key"
listen_port       = 6404
cluster_port_base = 6405    # シャード i の暗号化ポートは 6405 + i。0 = listen_port + 1
```

暗号化ポートから入ったクライアントには暗号化ポートが示されます。`-MOVED` と `CLUSTER SLOTS` / `NODES` / `SHARDS` は双子のポートを返し、平文クライアントには今までどおり平文ポートが示されます。プロキシや NAT の後ろでは、`announce_cluster_port_base` で通知する先頭ポートを設定します。暗号化ポートの範囲どうし、およびクライアントポートや平文クラスタポートと重なってはならず、重なるとサーバーは起動しません。

`kevy_client::ClusterClient::connect_url`、`kevy_client_async::cluster::AsyncClusterClient::connect_secure_url`、`kevy-cli -c`、`kevy-cli --cluster` ツールは、暗号化クラスタポートの 1 つを指す `kevys://` URL を受け取り、同じ鍵ですべてのシャードに接続します。

```text
kevy-cli -c -u "kevys://10.0.0.11:6405?server_key=<hex>" SET user:1 alice
kevy-cli -u "kevys://10.0.0.11:6405?server_key=<hex>" --cluster info 10.0.0.11:6405
```

`kevy_cluster_rw::ReadWriteClient::connect_urls` はノードごとに URL を 1 つ受け取り、それぞれにそのノードの鍵を付けます。暗号化時には、鍵を持たないノードへの `-MISDIRECTED` を平文でたどらず、拒否します。

## コスト

暗号処理はリアクタとは別のスレッドで動きます。各接続のバイトはそこで復号されてループバック経由で平文ポートに渡され、応答は戻る途中で暗号化されます。このポートを開いても開かなくても平文の経路は同じコードで、暗号化接続は暗号処理のほかにループバックの往復を 1 回余分に払います。

1 台の Linux ホストで、クライアントとサーバーをループバックで結び、4 シャードで計測しました。

| | 平文ポート | 暗号化ポート |
|---|---:|---:|
| 1 リクエストの往復 | 10 µs | 25 µs |
| 1 接続での 256 KB `GET` | 3.0 GB/s | 0.32 GB/s |
| 新規接続と `PING` 1 回 | 38 µs | 0.58 ms |

往復で増えた時間の大半はその余分な 1 ホップで、小さなメッセージの暗号化と復号は合わせて約 0.5 µs です。大きな値は移植性のある実装の暗号で頭打ちになり、ハンドシェイクの時間はほぼ X25519 です。接続を開いたまま使うコネクションプールなら、ハンドシェイクは 1 回で済みます。

## 暗号化ノードと平文ノードの混在

暗号化ノードは平文ノードと通信しません。平文のレプリカが暗号化プライマリに接続しても応答はなく、他ノードの `peer_keys` にない鍵を持つノードの声は選挙で誰にも届きません。クラスタを暗号化に切り替えるには、新しい設定で全ノードを再起動してください。

## 実測したこと

3 ノード、各 1 シャードで、選挙とレプリケーションの両方を暗号化した構成です。

- プライマリへの書き込みは両方のレプリカに届き、ノード間ネットワークのキャプチャには、200 回書いたマーカー値がレプリケーションと選挙のポートで一度も現れませんでした。同じ手順を `secure = false` で行うと 400 回現れました。
- プライマリを停止すると新しいプライマリが選ばれ、もう一方のノードがそれに追従し、フェイルオーバー後の書き込みも受け取りました。
- ノードあたり 4 シャード、io_uring リアクタの構成で、`replica_keys` で制限したレプリカは書いた 400 個のキーをすべて受け取り、レプリケーションポートのキャプチャには値が一つもありませんでした。同じ構成の平文での実行では 400 個すべてが見えました。
- 別のプライマリ鍵を期待するレプリカ、`replica_keys` にないレプリカ、平文のレプリカ、設定されていない鍵を持つ選挙ピアは、いずれも拒否されました。

## 参照

- [deploy-behind-a-proxy.md](deploy-behind-a-proxy.md)：`kevys://` を使えないクライアントの TLS
- [replication.md](replication.md)：レプリケーションそのもの
- [availability.md](availability.md)：選挙とフェイルオーバー

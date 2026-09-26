# kevy ノード間の暗号化リンク

レプリケーションと選挙のコントロールプレーンは、kevy 自身の、暗号化され相互に認証されたリンクの上で動かせます。どちらも**明示的に有効にしない限りオフ**で、有効にしてもクライアント接続には何も影響しません。クライアントに対して kevy は今も TLS も AUTH も持たず、クライアントは [deploy-behind-a-proxy.md](deploy-behind-a-proxy.md) のとおりプロキシ経由で接続します。

## 対象となるリンク

| リンク | 暗号化される条件 | 認証される相手 |
|---|---|---|
| 選挙（`elect_port_base`） | `[cluster] secure = true` | 両端。`peer_keys` の鍵による |
| レプリケーション（`listen_port_base + i`） | `[replication] secure = true` | プライマリは `upstream_key` かピアの鍵で、レプリカは `replica_keys` を設定したときに認証される |
| クライアント（`port`、クラスタポート、unix ソケット） | 暗号化しない | 認証しない。プロキシを使う |

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
        .with_writer_security(LinkKeys { local: writer_key, peers: vec![] }),
)?;

// レプリカ：ライターの公開鍵を信頼する
let replica = Store::open(
    Config::default()
        .without_aof()
        .with_replica_upstream("writer.internal:7101")
        .with_replica_security(LinkKeys { local: replica_key, peers: vec![writer_public] }),
)?;
# Ok::<(), kevy_embedded::KevyError>(())
```

- レプリカでは、`peers` は信頼するプライマリの一覧で、応答があるまで順に試します。空だと開くときに失敗します。ライターでは、接続を許すレプリカの一覧で、空ならすべて許可します。
- `Keypair::from_secret` が受け取る 32 バイトは、`kevy keygen` が hex で書き出すものです。どこに保管するかはアプリケーションが決めます。

## 暗号化ノードと平文ノードの混在

暗号化ノードは平文ノードと通信しません。平文のレプリカが暗号化プライマリに接続しても応答はなく、他ノードの `peer_keys` にない鍵を持つノードの声は選挙で誰にも届きません。クラスタを暗号化に切り替えるには、新しい設定で全ノードを再起動してください。

## 実測したこと

3 ノード、各 1 シャードで、選挙とレプリケーションの両方を暗号化した構成です。

- プライマリへの書き込みは両方のレプリカに届き、ノード間ネットワークのキャプチャには、200 回書いたマーカー値がレプリケーションと選挙のポートで一度も現れませんでした。同じ手順を `secure = false` で行うと 400 回現れました。
- プライマリを停止すると新しいプライマリが選ばれ、もう一方のノードがそれに追従し、フェイルオーバー後の書き込みも受け取りました。
- ノードあたり 4 シャード、io_uring リアクタの構成で、`replica_keys` で制限したレプリカは書いた 400 個のキーをすべて受け取り、レプリケーションポートのキャプチャには値が一つもありませんでした。同じ構成の平文での実行では 400 個すべてが見えました。
- 別のプライマリ鍵を期待するレプリカ、`replica_keys` にないレプリカ、平文のレプリカ、設定されていない鍵を持つ選挙ピアは、いずれも拒否されました。

## 参照

- [deploy-behind-a-proxy.md](deploy-behind-a-proxy.md)：クライアント接続の TLS
- [replication.md](replication.md)：レプリケーションそのもの
- [availability.md](availability.md)：選挙とフェイルオーバー

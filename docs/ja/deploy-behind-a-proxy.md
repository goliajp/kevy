# kevy をプロキシの後ろに置いて運用する

kevy に AUTH はなく、TLS もありません。**認証も暗号化も、プロセスの前で行います。** この章はその手順で、書き換えずにそのまま写して使えるように書いています。（Rust クライアントは代わりに、`kevys://` で kevy 自身の暗号化クライアントポートを使えます。[encrypted-links.md](encrypted-links.md) を参照。）

## kevy が開くもの

| | ポート | 備考 |
|---|---|---|
| 既定 | **ひとつ**（`6004`） | `--threads N` は**同じ**ポートに `SO_REUSEPORT` のリスナを N 個開くのであって、N 個のポートではありません |
| `--cluster` | **1 + N** | メインポートに加えて、シャードごとに `port+1+i` |
| `KEVY_UNIX_SOCKET=<path>` | 変わらない | unix ソケットは**追加**されるだけで、TCP のリスナはそのまま |

既定のバインドは `127.0.0.1` です。これがすでにこの章の求める形です。エンジンはこのホストからしか届かない場所で待ち受け、公開アドレスを持つのは前段の終端器だけになります。

## 形

```
   client ──TLS──▶  terminator  ──plain──▶  kevy
  (rediss://)     (stunnel / HAProxy /     127.0.0.1:6004
   + クライアント証明書  nginx stream)       または /run/kevy/kevy.sock
```

kevy 側は何も変わりません。RESP にはホスト名も SNI も絶対 URL も載らないので、前段のバイトプロキシはどちら側からも見えません。

## RESP は HTTP ではない

HTTP のリバースプロキシは RESP を運べません。**素の Caddy** も同じです。コアに L4 モジュールがないので、Caddyfile をどう書いても `caddy` 単体では kevy の前に TLS を置けません。TCP レベルの終端器が必要です。

- **stunnel**：いちばん小さく、どのディストリビューションにもパッケージがあり、この仕事だけをします。
- **HAProxy** の `mode tcp`：すでに HAProxy を運用しているか、ヘルスチェックやフェイルオーバーを同じ場所で扱いたいときに。
- **nginx** の `stream` モジュール：同様に、すでに nginx があるときに。

以下の設定はどれも、暗号化に加えて**あなたの CA が発行したクライアント証明書を要求します**。クライアント証明書の行を外すと、プロキシは接続してきた誰に対しても暗号化するだけになります。kevy には AUTH がないので、それは誰でもすべてのキーを読み書きできるということです。

### stunnel → ループバックのポート

```ini
[kevy]
accept      = 0.0.0.0:6379
connect     = 127.0.0.1:6004
cert        = /etc/kevy/tls/server.crt
key         = /etc/kevy/tls/server.key
CAfile      = /etc/kevy/tls/ca.crt
verifyChain = yes
requireCert = yes
```

### HAProxy → unix ソケット

ソケットファイルはファイルシステムの権限で囲えます。kevy はソケットを誰でも書き込める状態で作るので、終端器のグループだけが入れるディレクトリ、たとえばモード `0750` の `/run/kevy` に置いてください（[uds.md](uds.md) を参照）。ただし、これで終端器が唯一の入口になるわけではありません。後述のループバックの TCP リスナは開いたままなので、共有ホストではどのローカルユーザーも `127.0.0.1:6004` に届きます。

```
listen kevy
    bind :6379 ssl crt /etc/kevy/tls/server.pem ca-file /etc/kevy/tls/ca.crt verify required
    mode tcp
    timeout client 0
    timeout server 0
    server kevy unix@/run/kevy/kevy.sock
```

`server.pem` は証明書の後ろに秘密鍵をつなげた 1 つのファイルです。

ソケット付きで kevy を起動します。

```console
KEVY_UNIX_SOCKET=/run/kevy/kevy.sock kevy --dir /var/lib/kevy
```

このパスについて 2 点あります。パスがすでに存在すると **kevy は起動を拒否します**。自分で作っていないパスは上書きしないので、再起動時に片付けるか、起動ごとに別のパスを使ってください。また、`127.0.0.1` の TCP リスナはそのまま残ります。ソケットは置き換えではなく追加です。

### nginx stream → unix ソケット

```nginx
stream {
    upstream kevy { server unix:/run/kevy/kevy.sock; }
    server {
        listen 6379 ssl;
        ssl_certificate        /etc/kevy/tls/server.crt;
        ssl_certificate_key    /etc/kevy/tls/server.key;
        ssl_client_certificate /etc/kevy/tls/ca.crt;
        ssl_verify_client      on;
        proxy_pass kevy;
        proxy_timeout 1h;
    }
}
```

3 つの設定のタイムアウトに注意してください。ブロック中の `BLPOP` やアイドルな Pub/Sub の購読者は、1 バイトも流れない接続を、アプリケーションが望むだけ開いたままにします。アイドル接続を刈り取るプロキシは、**kevy が購読者を落としているのとまったく同じに見えます**。nginx の `proxy_timeout` は無効にできないので、想定される最長のアイドル時間より長くしてください。

## クライアント証明書

CA は終端器に信頼されればよいので、自前のもので足ります。OpenSSL での例です。

```console
# CA：ca.key はオフラインで保管
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=kevy-ca" \
  -keyout ca.key -out ca.crt

# 終端器の証明書。クライアントが接続に使う名前で発行する
openssl req -newkey rsa:2048 -nodes -subj "/CN=kevy.internal" -keyout server.key -out server.csr
printf 'subjectAltName=DNS:kevy.internal\n' > san.ext
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 825 \
  -extfile san.ext -out server.crt
cat server.crt server.key > server.pem      # HAProxy 用の形式

# アプリケーションごとに 1 枚
openssl req -newkey rsa:2048 -nodes -subj "/CN=billing-app" -keyout billing.key -out billing.csr
openssl x509 -req -in billing.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 825 \
  -out billing.crt
```

あるアプリケーションを失効させるには、新しい CA で発行し直すか、終端器に CRL を追加します。kevy 自身は証明書を一切見ません。

## クライアント側

TLS を有効にした標準の Redis クライアントなら、終端器の後ろの kevy にそのまま接続できます。終端器を信頼するための CA と、入るための自分の証明書が必要です。

```console
redis-cli --tls --cacert ca.crt --cert billing.crt --key billing.key -h kevy.internal -p 6379 PING
```

**`kevy-cli` はできません。** `rediss://` に対して `Unsupported` を返します。kevy は TLS なしで出荷され、CLI にも使える TLS 実装がないからです。使う段になって気づくのではなく、先に段取りしておくべき運用上の帰結です。ホスト上で直接操作するか、kevy 自身の暗号化クライアントポート（`kevy-cli -u kevys://…`。[encrypted-links.md](encrypted-links.md) を参照）を使うか、SSH トンネルを使ってください。

```console
ssh -N -L 6004:127.0.0.1:6004 you@host   # そのあと：kevy-cli -p 6004
```

## 必要なものだけを晒す

既定のバインドなら、**kevy にファイアウォール規則は一つも要りません**。そもそもホストの外から届かないからです。開けるポートは終端器の分だけです。どうしても kevy を実インタフェースにバインドするなら、ループバックのバインドが何もせずに果たしていた役目をファイアウォールが引き受けることになり、それがネットワークと**認証のないデータベース**の間にある唯一の防壁になります。

## プロキシの後ろでクラスタモードを使う

キーを意識するクライアントは、各スロットの場所を `CLUSTER SLOTS` から知り、`-MOVED` のリダイレクトに従います。したがって kevy は自分のアドレスではなく、**プロキシ**が待ち受けるアドレスを広告する必要があります。そのための設定が 2 つあります。

```toml
[cluster]
enabled            = true
announce_ip        = "203.0.113.7"   # クライアントがプロキシに到達するアドレス
announce_port_base = 7001            # プロキシ側でシャード 0 に割り当てるポート
```

そのうえでシャードごとのポートを同じ順序で一対一に対応させます。プロキシの `7001 + i` を kevy の `port + 1 + i` の前に置きます。2 シャードを HAProxy の後ろに置く例です。

```
listen kevy-main
    bind :7000 ssl crt /etc/kevy/tls/server.pem ca-file /etc/kevy/tls/ca.crt verify required
    mode tcp
    server kevy 127.0.0.1:6004
listen kevy-shard0
    bind :7001 ssl crt /etc/kevy/tls/server.pem ca-file /etc/kevy/tls/ca.crt verify required
    mode tcp
    server kevy 127.0.0.1:6005
listen kevy-shard1
    bind :7002 ssl crt /etc/kevy/tls/server.pem ca-file /etc/kevy/tls/ca.crt verify required
    mode tcp
    server kevy 127.0.0.1:6006
```

これで `CLUSTER SLOTS`、`CLUSTER NODES`、`CLUSTER SHARDS` とすべての `-MOVED` が `203.0.113.7:7001` と `203.0.113.7:7002` を示すようになります。`announce_ip` がなければ、kevy はバインドしているアドレスを広告し、`0.0.0.0` のバインドなら `127.0.0.1` を広告します。同じホストのクライアントには正しくても、ほかの場所からは届きません。

## ホスト間のレプリケーションと選挙

レプリケーションと選挙のコントロールプレーンはそれぞれ専用のポートを持ちます。kevy 自身がこの両方を暗号化・認証できます（トンネル不要。[encrypted-links.md](encrypted-links.md) を参照）。それを使わない場合、どちらも暗号化も認証もされません。ホストをまたぐ場合は、各ノードをループバックでだけ待ち受けさせ、ピアごとにローカルのトンネル入口を用意します。各ノードに必要なものは次のとおりです。

- 受信側：このノードが提供するローカルポートごとに 1 つの TLS サービス。クライアントポート（`FAILOVER` の問い合わせ用）、選挙ポート、そして**シャードごとに 1 つ**のレプリケーションポート。
- 送信側：各ピアのそれらのポートごとに 1 つのクライアントモードのサービス。ローカルポートは自由に選べます。
- `peers` リストでは各ピアを**ローカル**のトンネルポートで書き、レプリケーションポートは 4 つ目のフィールドに入れます。こうすると、新しく選ばれたプライマリに追従するノードは、ピアに直接ではなくトンネルに接続します。

3 ノードのうち `n2` の例です。各ノード 1 シャードで、ピア `n1` と `n3` をそれぞれローカルポート `8010-8012` と `8030-8032` に割り当てます。

```toml
[server]
bind = "127.0.0.1"
port = 6004

[replication]
role     = "replica"
upstream = "127.0.0.1:8012"          # n1's replication port, through the tunnel

[cluster]
enabled         = true
node_id         = "n2"
elect_port_base = 6204
peers = "n1@127.0.0.1:8011:8010:8012,n2@127.0.0.1:6204:6004:16004,n3@127.0.0.1:8031:8030:8032"
```

```ini
foreground = yes

; inbound: what peers reach on this host
[in-client]
accept      = 0.0.0.0:7004
connect     = 127.0.0.1:6004
cert        = /etc/kevy/tls/server.crt
key         = /etc/kevy/tls/server.key
CAfile      = /etc/kevy/tls/ca.crt
verifyChain = yes
requireCert = yes
; [in-elect] 7204 -> 6204 and [in-repl] 17004 -> 16004, same options

; outbound: n1's three ports as local ports on this host
[to-n1-client]
client      = yes
accept      = 127.0.0.1:8010
connect     = n1.internal:7004
cert        = /etc/kevy/tls/n2.crt
key         = /etc/kevy/tls/n2.key
CAfile      = /etc/kevy/tls/ca.crt
verifyChain = yes
checkHost   = n1.internal
; [to-n1-elect] 8011 -> n1.internal:7204, [to-n1-repl] 8012 -> n1.internal:17004,
; and the same three for n3 on 8030-8032
```

TLS のオプションは上のように各サービスに書いてください。stunnel 5.76 は、これらをグローバルセクションに置き、同じファイルにクライアントモードのサービスもあると、起動時にクラッシュしました。

実測したこと：3 ノード、各ノード 1 シャードで、すべてのリンクを stunnel とクライアント証明書経由にした構成。プライマリへの書き込みは両方のレプリカに届き、ホスト間のネットワークのキャプチャには 200 回書いたマーカー値が一度も現れませんでした。同じ時間にプライマリのループバックで取ったキャプチャには 600 回現れています。プライマリを停止すると、`n2` が 6 秒で選ばれ、`n3` はローカルのトンネル経由で追従し、フェイルオーバー後の書き込みも受け取りました。実測していないこと：ノードあたり 2 シャード以上の構成。

## 実測したこと、していないこと

このツリーで、HAProxy 3.4.5、nginx 1.30.5、stunnel 5.76 を kevy の前に置き、クライアントに redis-cli 8.0.2、証明書の発行に OpenSSL 3.5.7 を使って実測しました。

- 上の表のポート構成。`--cluster` がシャードごとに `port+1+i` を開くことも含む。
- 上の 3 つのクライアント証明書の設定。いずれも kevy の TCP ポートへ転送する形で確認した。CA が発行した証明書を持つクライアントは読み書きでき、証明書のないクライアントと別の CA の証明書を持つクライアントはコマンドを送る前に切断され、書こうとしたキーは後から見つからない。
- HAProxy と nginx の設定で kevy の unix ソケットへ転送する形。受け入れと拒否の結果は上と同じ。kevy が作ったソケットは `srwxrwxrwx` だったので、ディレクトリで囲う必要がある。
- 上のタイムアウト設定で、3 つの終端器それぞれを通して `BLPOP` を 10 分間アイドルのまま保ち、その後 push で解放した。3 つとも push した要素を受け取った。
- `announce_ip` と `announce_port_base` を設定し、HAProxy の後ろに置いたクラスタモード。`-MOVED` はプロキシ側のシャードポートを示し、`redis-cli -c` で 1 つのシャードポートから書いたキーは、メインポートからすべて読み戻せる。
- 素の Caddy（2.11.4）には L4 モジュールがない。
- `kevy-cli` が `rediss://` を拒否する。

試していないこと：CRL によるクライアントの失効と、期限切れのクライアント証明書。

## 参照

- [uds.md](uds.md)：unix ソケットの詳細
- [cluster.md](cluster.md)：単一ノードのクラスタモードが何のためにあるか
- [tuning.md](tuning.md)：`--threads`、そしてスレッドを減らしたほうが速くなりうる理由

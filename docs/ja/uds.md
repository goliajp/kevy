# Unix-domain socket (UDS) トランスポート

kevy はオプションの Unix-domain stream リスナーを公開しており、TCP ポートと同一の RESP セマンティクスを話します。同一ホストのクライアントはこれでループバックスタックを完全にスキップできます。

## このドキュメントが必要になるとき

クライアントとサーバーが同じホストを共有するときに UDS は適切な選択です:

- **同一ホストのクライアント** — 1 台のマシン上でアプリと kevy、あるいは tmpfs / マウント済みソケットディレクトリを共有するコンテナ。
- **遅延に敏感なワークロード** — 低コネクション数、小ペイロード、または TCP ループバック往復のフロアが制約になっている高ファンアウトのパイプライニング。
- **コンテナサイドカー** — サイドカー + メインコンテナが `/run` または `/tmp` ボリュームを共有。ソケットファイルが IPC ハンドルで、ポート割り当ては不要。

クロスホストのクライアントは依然 TCP が必要です — UDS はファイルシステムスコープで、カーネルから出ません。

## 中心となる考え方

`KEVY_UNIX_SOCKET` をファイルシステムのパスに設定すると、kevy は dual-bind します: TCP リスナーはこれまで通りそのまま生きており、UDS リスナーが同じシャードランタイム上で同じ RESP2/3 パーサーで accept します。`unix://` URL または `-s <path>` フラグを取る RESP クライアントは config 1 行で切り替えられます。UDS はループバックの `rep_movs`、`nft_do_chain`、TCP syscall パスをなくすので、各ワークロードで op あたりのフロアが目に見えて下がります。

## 動かしてみる例

両方のトランスポートを有効にして kevy を起動:

```sh
KEVY_UNIX_SOCKET=/tmp/kevy.sock kevy --port 6379
```

`redis-cli` で UDS 経由接続:

```sh
redis-cli -s /tmp/kevy.sock SET foo bar
# OK
redis-cli -s /tmp/kevy.sock GET foo
# "bar"
```

TCP の `:6379` も並行で生きています — 同じデータ、同じシャード:

```sh
redis-cli -p 6379 GET foo
# "bar"
```

Rust では、リポジトリ内クライアントが `unix://` URL を受け取ります:

```rust
let mut conn = kevy_client::Connection::open("unix:///tmp/kevy.sock")?;
conn.set(b"k", b"v")?;
```

## 権限とセキュリティ

UDS の信頼境界は**ファイルシステム**です — Unix ソケット上に RESP レベルの AUTH や TLS はありません。ソケットファイルを `open(2)` できる誰でも、任意のコマンド(`FLUSHALL` 含む)を発行できます。

- **ソケットファイルの所有者。** kevy はサーバーが動いているユーザーとしてソケットを作ります。起動後 `chown` / `chgrp` するか、ソケットを所有させたい ID で kevy を動かしてください。
- **パーミッションビット。** 同居するクライアントプロセスが接続できるよう、デフォルトでは緩いビットでソケットを作ります。引き締めたければ、ソケットを制限付きディレクトリ — 例えば `kevy` グループ所有の `0750` の `/run/kevy/` — に置いて、グループメンバだけが `connect(2)` できるようにします。ディレクトリ権限がソケット inode 自体へのアクセスを守ります。
- **tmpfs vs ディスク。** ほとんどの Linux ディストロの `/tmp` と `/run` は tmpfs で、ソケットには理想的です(connect 時のディスク I/O なし)。実ファイルシステム上の永続パスでも動きます — inode はランデブー点にすぎず、データがディスクに触れることはありません。
- **信頼ドメイン。** ソケットパスへの読み書きを持つアカウントはすべて完全認証済みとして扱ってください。クライアントごとの ID が必要なら、それは kevy より上(サイドカープロキシ、カーネル LSM、名前空間分離)に居る必要があります。

## サーバー設定ノブ

| 環境変数 | CLI フラグ | デフォルト | 効果 |
|---|---|---|---|
| `KEVY_UNIX_SOCKET` | (今のところ env 専用) | 未設定 | bind するファイルシステムパス。未設定で TCP のみ。 |
| `KEVY_BIND` | `--bind` | `127.0.0.1` | TCP の bind アドレス。UDS の bind は独立。 |
| `--port` | `--port` | `6379` | TCP ポート。設定時も UDS は bind される。 |

注意:

- **パスは事前に存在してはいけません。** kevy は `KEVY_UNIX_SOCKET` が既存ファイルを指している場合は起動を拒否します — 自分が作ったのではないパスを上書きしません。再起動時に掃除する(`rm -f /tmp/kevy.sock`)か、run ごとのパス(`/run/kevy/$(date +%s).sock`)を使ってください。これは意図的で、黙って unlink すると誤設定の kevy が他サービスのソケットを奪い得るためです。
- **環境変数が設定されていれば常に dual-bind。** UDS のみのモードはありません — TCP リスナーも上がります。TCP を禁止したければ、制御できるループバック専用アドレスに bind し、firewall で塞いでください。
- **シャード 0 が accept ループを所有します。** accept された接続は既存のシャード別ランタイムへディスパッチされるので、`--threads` は依然ソケット越しのワークロードの並列性を制御します。
- **io_uring パス。** Linux 上 `KEVY_IO_URING=1` のとき、UDS の accept は TCP と同じ io_uring インスタンスを通る multishot accept SQE として動きます — 余計な reactor コストなし。`TCP_NODELAY` は UDS には設定されません(IP ソケットではないので)。

## トレードオフ

同じ kevy バイナリ上での UDS vs TCP ループバック:

| 側面 | UDS | TCP ループバック |
|---|---|---|
| op あたりのフロア | 低い(IP/checksum/port/NAGLE なし) | 高い |
| 到達範囲 | 同一ホストのみ | 任意のホスト |
| ID | ファイルシステムのパーミッション | port + bind アドレス + AUTH |
| ライフサイクル | ディスク上のソケットファイル。再起動時に掃除必要 | ポートのライフサイクルはカーネル管理 |
| 観測 | `lsof` / `ss -xl` | `ss -tln`、`netstat`、`tcpdump` |
| クライアント設定 | `unix:///path` または `-s /path` | `host:port` |

スループットの利得はワークロード形状依存です — 小ペイロード低コネクション数のセルが最も得をします(ループバックの op あたり税が支配的だった)。CPU 飽和セルの利得は小さくなります(トランスポートがフロアではなかった)。実測値は [bench/REPORT.md](https://github.com/goliajp/kevy/blob/master/bench/REPORT.md) を参照。

## FAQ

### UDS と TCP を同時に bind できますか?

はい — それが唯一のモードです。`KEVY_UNIX_SOCKET` を設定すると UDS リスナーが足され、TCP リスナーはそのまま生きます。クライアントごとに筋の通る方を使ってください。

### サーバーが「socket exists」と言って起動を拒否します。

意図的です。kevy は自分が作っていないパスを `unlink` しません。誤設定の run が他サービスのソケットを黙って奪うのを防ぐためです。再起動前に古いファイルを消す(`rm -f /tmp/kevy.sock`)か、`/run/kevy/$(uuidgen).sock` のような run ごとのパスを使ってください。kevy がクラッシュしてファイルを残したなら手で消すのは安全です。

### UDS は TCP ループバックよりどれくらい速いですか?

各ワークロードで目に見えて速いです。UDS は IP パス全体をスキップするからです: checksum なし、netfilter チェーン(`nft_do_chain`)なし、ループバックの `rep_movs` なし、パケットごとの ACK 往復なし。比率は op 予算のうちループバックオーバーヘッドがどれくらいを占めていたかに依存します — 単一コネクション・小ペイロードが最大の跳ね、CPU バウンドでパイプライニングしたセルは小さくなります。あなたのワークロードでの計測は `redis-benchmark -s /tmp/kevy.sock` vs `-h 127.0.0.1` で。

### 自分のクライアントライブラリは UDS を使えますか?

多くが使えます。`redis-cli` と `redis-benchmark` は `-s <path>` を取ります。ioredis、node-redis、redis-py、redis-rb、go-redis、lettuce、jedis、およびリポジトリ内の [kevy-client](https://github.com/goliajp/kevy/tree/master/crates/kevy-client) / [kevy-client-async](https://github.com/goliajp/kevy/tree/master/crates/kevy-client-async) はすべて `unix:///path` URL または明示的なソケットパスオプションを受けます。正確なキー名はドライバの接続オプション docs で確認してください。

### 全クライアントが同一ホストにあるなら TCP を完全に外すべきですか?

外せますが、必須ではありません。TCP を `127.0.0.1` に bind したままにするのは誰も繋がなければコストゼロで、もしクライアントの UDS パスが誤設定になってもフォールバックになります。よくあるデプロイは「ホットなクライアントには UDS、`redis-cli` デバッグには TCP」です。

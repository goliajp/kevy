# Unix-domain socket (UDS) トランスポート

v1.25 で opt-in の Unix-domain stream socket listener を追加 ——
valkey/redis の `unixsocket` 設定に相当します。同一ホスト上の
ローカル・クライアント(server プロセスと同じ信頼境界)向けに、
UDS は TCP loopback スタックを完全に飛ばします:IP ヘッダ無し、
checksum 無し、ポート照合無し、NAGLE/ACK の応酬無し。ワイヤは
RESP2 / 3 でバイト等価なので、既存クライアントは URL を 1 つ変える
だけで切り替わります。

## いつ使うか

UDS は次の **3 つすべて** が成立するときに適切:

1. クライアントが server と**同一ホスト**(共有 tmpfs / host volume
   をマウントしたコンテナも該当)。
2. per-syscall のネットワーク・オーバーヘッドで CPU bound —— 小さな
   payload、多接続、シングル shard server、または `-c1` 系で
   per-op RTT をフルに払うワークロード。
3. 信頼境界が**ホストのファイルシステム**(UDS のパーミッションは
   ファイルシステム・パーミッション;kevy と valkey どちらにも
   AUTH/TLS は無い)。

UDS は TCP の**代替にならない**ケース:

- クライアントが別コンテナで、共有 socket マウントが**ない**
  (`/tmp/kevy.sock` パスが両側から見えなければならない)。
- ネットワーク到達性が必要 —— リモート・クライアントは TCP
  loopback のみ(kevy は単 DC、公インターネット設計なし)。
- ワークロードが `-c50 -P16` pipelined で既に server の CPU を
  飽和済み —— UDS でも数 % は取れるが、テコは transport ではない。

なぜ kevy の UDS 利得が valkey より大きいか / Phase A 分解の経緯
は [`bench/REPORT.md`](../../bench/REPORT.md)。

## サーバ設定

起動前に `KEVY_UNIX_SOCKET` をファイルパスに設定。server は TCP
listener と**並行で UDS をバインド**します —— 両方が並列で accept
し、クライアント側でどちらを使うか選ぶ:

```sh
KEVY_UNIX_SOCKET=/tmp/kevy.sock kevy --port 6004
```

挙動:

- bind 前にパスを `unlink`(前回クラッシュで残った stale socket は
  自動掃除 —— valkey/redis と同じ)。
- bind 後に `chmod 0777`(任意のローカル・ユーザが接続可能;
  per-user アクセス制御は包含ディレクトリのパーミッションで)。
- **shard 0 のみ** が UDS listener を持ち、accept された接続は
  既存の per-shard runtime に振り分けられます。つまり `--threads`
  設定は socket 越しのワークロードの並列度を依然として制御します。
- Linux で `KEVY_IO_URING=1` を設定すると、UDS accept loop は TCP
  と同じ io_uring インスタンスで multishot accept SQE として動き
  ます —— reactor の追加コスト無し。UDS は IP socket ではないので
  TCP_NODELAY はスキップ。
- 空 / 未設の `KEVY_UNIX_SOCKET` = TCP のみ(v1.24 以前の挙動)。

CLI / TOML の等価項は v1.26 で予定;現状は環境変数 1 つのみ。

## クライアント設定

Unix-socket オプションを取る Redis/RESP クライアントはそのまま
動きます —— RESP2/3 フレーミング同一。

`redis-cli` / `redis-benchmark`(`-s` フラグ):

```sh
redis-cli -s /tmp/kevy.sock SET foo bar
redis-cli -s /tmp/kevy.sock GET foo
redis-benchmark -s /tmp/kevy.sock -t set,get -n 100000 -c 50 -P 16
```

[`kevy-client`](../../crates/kevy-client) と
[`kevy-client-async`](../../crates/kevy-client-async) は `unix://`
URL を受け付けます:

```rust
let mut conn = kevy_client::Connection::open("unix:///tmp/kevy.sock")?;
conn.set(b"k", b"v")?;
```

valkey / redis 相当設定(`unixsocket` ディレクティブ):

```sh
valkey-server --unixsocket /tmp/valkey.sock --unixsocketperm 777 \
              --io-threads 10
redis-server  --unixsocket /tmp/redis.sock  --unixsocketperm 777
```

## ベンチ数字

Precision bench、n=1 M × 10 runs、2σ フィルタ平均、全セルで CI95
< 1 %。lx64、`mitigations=off`、kevy `--threads 1`(シングル shard)、
valkey `--io-threads 10`。再現:
[`bench/v125-precision-uds.sh`](../../bench/v125-precision-uds.sh)。

| ワークロード | kevy 1.25 (UDS) | valkey 9.1 (UDS) | kevy / valkey |
|------------|----------------:|-----------------:|--------------:|
| -c1 SET | **166 k/s** | 96 k/s | **1.73×** |
| -c1 GET | **168 k/s** | 106 k/s | **1.59×** |
| -c50 -P1 SET | 339 k/s | 334 k/s | タイ(per-syscall 床) |
| -c50 -P1 GET | 337 k/s | 332 k/s | タイ(per-syscall 床) |
| **-c50 -P16 SET** | **4.11 M/s** | 1.75 M/s | **2.35×** |
| **-c50 -P16 GET** | **4.35 M/s** | 3.42 M/s | **1.27×** |
| -c100 -P1 SET | 331 k/s | 326 k/s | タイ |
| -c100 -P1 GET | 335 k/s | 327 k/s | タイ(1.02×) |

UDS vs TCP for kevy(同 server、同 bench、transport だけ切替え):

| ワークロード | TCP rps | UDS rps | UDS / TCP |
|------------|--------:|--------:|----------:|
| -c1 SET | 94.7 k | 166 k | **1.76×** |
| -c1 GET | 97.3 k | 168 k | **1.73×** |
| -c50 -P1 | 192 k | 339 k | **1.77×** |
| -c50 -P16 SET | 2.59 M | 4.11 M | **1.59×** |
| -c50 -P16 GET | 2.67 M | 4.35 M | **1.63×** |

なぜ kevy の UDS 利得が valkey より大きいか:valkey の hot path は
より CPU bound(`processCommand` / `addReply` の per-op 仕事)で、
TCP 天井がそもそも transport の RTT 床を下回っていた —— loopback
を取り除いても valkey に余地はあまり生まれません。kevy の hot
path は十分に軽いので、`-c50 -P16` での束縛は TCP RTT 床でした;
UDS で束縛が外れると server は loadgen より先に走り切ります。
c=50/100 -P1 のタイは UDS 上でもタイのまま —— 両 server とも
per-syscall round-trip 床(~3 µs × 50 conn)で飽和しており、
transport とは無関係。

## セキュリティ注意

- **ファイル・パーミッション = AUTH 相当。** UDS にネイティブの認証
  はありません;socket ファイルを `open(2)` できる相手なら任意の
  コマンド(`FLUSHALL` 含む)を発行できます。kevy 既定の
  `chmod 0777` は valkey/redis 既定と同じ;絞りたいときは
  socket を制限的パーミッションのディレクトリに置く(例:
  `/run/kevy/kevy.sock` を `kevy` グループ所有)。
- **クラッシュ後の stale socket。** kevy は bind 前に `unlink`
  するので前回クラッシュの残骸が起動を妨げません。同一パスを 2 つの
  kevy インスタンスが指せば後勝ち —— 前者のクライアントは次の write
  で `EPIPE`。
- **リモート不可。** UDS はホスト・ローカル。クロス・ホストの
  クライアントは TCP のみ(kevy は依然として単 DC・AUTH/TLS なし
  —— [`README.ja.md`](../../README.ja.md))。

## 再現

```sh
ssh lx64
bash /path/to/kevy/bench/v125-precision-uds.sh
```

precision harness は同じ kevy バイナリをビルドし、kevy と valkey
を順に立ち上げ、`redis-benchmark -s <sock>` を 10 回 × n=1 M 走らせ、
フィルタ平均と CI95 を表示します。お供の smoke
[`bench/v125-uds-smoke.sh`](../../bench/v125-uds-smoke.sh)(14
テスト・グループ、39 アサーション、SET/GET、各コレクション、
INCR/APPEND、大きな値、SETEX、MSET、pipelined DBSIZE、pub/sub、
FLUSHALL、INFO をカバー)で UDS が TCP と wire 等価であることを
確認 —— server 側のコード経路は同じで、accept SQE のみ異なります。

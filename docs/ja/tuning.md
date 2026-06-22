# kevy を極限スループットまで詰める

kevy の op あたりオーバーヘッドを目に見えて動かすツマミを並べたページ
です。各ツマミに実測値(lx64、Intel Xeon 6、Linux 6.12 / io_uring;
方法論は [`bench/REPORT.md`](../../bench/REPORT.md))と明確なコストを
書いています。本当に必要なものだけ適用してください。

## 早見表

| ツマミ                            | いつ使う                          | 効果    |
|-----------------------------------|-----------------------------------|---------|
| server を CPU セットにピン留め    | 専用ホスト、bench 同居             | 5–15%   |
| AOF オフ (`--no-aof`)             | レプリカ / 揮発キャッシュ          | 5–10%   |
| `KEVY_IO_URING=1`                 | Linux 5.13+                        | 10–30%  |
| `--threads 1` を使う              | 単一クライアント / pipelined 負荷  | **5–60%** |
| **Unix-domain socket** に切替     | クライアントが同一ホスト           | **60–75%** |
| カーネル `mitigations=off`        | 信頼できる単一テナント機           | 12–25%  |
| netfilter ルール空                | 専用ホスト、ローカル FW 不要       | **25–35%** |
| PGO(profile-guided)              | ワークロード固定のリリース         | 1–10%   |

カーネル床を動かせるのは `mitigations=off` と netfilter 空の二つ;
UDS は loopback 床自体を取り除く;`--threads` は shard 数を負荷の並列
度に合わせる;PGO とそれ以外はユーザー空間サイクルを削るだけです。

## CPU ピン留め

io_uring の reactor は固定 CPU セットに留めると安定します —— NIC IRQ
→ softirq → ユーザースレッドが同じ L1/L2 にとどまるためです:

```sh
taskset -c 0-9 kevy --port 6004
```

同じマシンで bench を走らせる場合、**server と client は互いに重なら
ない範囲にピン留め** —— server を `0-9`、client を `10-15`(構成に応じ
て)。コアを共有するとスケジューラの取り合いが io_uring の効果を相殺
します。詳細は `feedback-kevy-bench-isolation`。

## `KEVY_IO_URING=1`

reactor を epoll から io_uring に切り替えます。Linux 5.13+ 必須、古い
カーネルでは静かに epoll にフォールバックします。lx64 で -c1 +10–30%、
SQPOLL (D5) の前提でもあります。

```sh
KEVY_IO_URING=1 kevy --port 6004
```

## 同一ホスト向けに Unix-domain socket (UDS)

クライアントが server と同一ホストにいるなら、ファイル経由の socket
を指し、TCP loopback スタックを完全に飛ばします。v1.25+。

```sh
KEVY_UNIX_SOCKET=/tmp/kevy.sock kevy --port 6004
redis-cli -s /tmp/kevy.sock SET foo bar
redis-benchmark -s /tmp/kevy.sock -t set,get -n 100000 -c 50 -P 16
```

server は TCP + UDS を**同時バインド** —— TCP はリモート、UDS は
ローカル。RESP セマンティクスも shard runtime も同一。lx64 precision
bench(同一バイナリ、同一クライアント、アドレスのみ変更):

| ワークロード | TCP rps | UDS rps | 改善 |
|------------|--------:|--------:|----:|
| -c1 SET | 94.7 k | 166 k | **+76 %** |
| -c1 GET | 97.3 k | 168 k | **+73 %** |
| -c50 -P16 SET | 2.59 M | 4.11 M | **+59 %** |
| -c50 -P16 GET | 2.67 M | 4.35 M | **+63 %** |

注意:UDS のパーミッションはファイル・パーミッション。既定の
`chmod 0777` は valkey/redis と同じです。サーバ上に信頼できない
ユーザがいる場合は包含ディレクトリで絞ってください。詳細(セキュリ
ティ、valkey 側相当設定、使うべきでないとき)は
[`docs/uds.md`](../uds.md)。

## `--threads` —— shard 数 vs 負荷の並列度

kevy は thread-per-core。`--threads N`(または `KEVY_THREADS`)で
N 個の shard を作り、keyspace を CRC16 hashtag で分割します。
**スレッドを増やせば常に速い、ではありません** —— 負荷の形で選ぶ:

| 負荷の形 | 推奨 | 理由 |
|---------|------|------|
| 単一接続 bench(`-c1 -P1`) | `--threads 1` | conn は 1 shard に張り付く;余の shard は空転で CPU を浪費 |
| 単一クライアント pipelined(`-c50 -P16`) | `--threads 1` | クライアント 1 コアで 1 shard を飽和;多 shard はクロス税を払う |
| 多独立クライアント、低 pipelining | `--threads ≤ コア数 / 2` | クライアントが散る;1 shard に 1 クライアント・コア |
| 混合(キャッシュ + クラスタ読) | `--threads = コア数 - 2` | OS / IRQ 用の余裕を残す |

v1.25 precision-bench headline はすべて `--threads 1` —— これが
redis-benchmark のクライアント側が天井に到達する構成です。同じ
`-c1` 負荷で `--threads 10` にすると **スループットが下がります**
(9 個の shard が無駄に busy-poll し、shard 0 の cache line を奪う)。

複数 shard のクロス・ルーティング詳細(`{hashtag}` slot、cluster
port、`ClusterClient`)は [`docs/cluster.md`](cluster.md)。

## BGSAVE / BGREWRITEAOF を bio スレッドで shard 外実行(v1.25)

v1.25 で snapshot + AOF rewrite は **server 全体で 1 つのグローバル
bio スレッド**(per-shard ではない)に移されました。shard は
`Op::Save` でリクエストをキューイングし、ネットワーク busy-poll を
継続;bio スレッドがホットパス外でディスク書き込みを実行します。

効果:shard の busy-poll の周期が数秒級のディスク書き込みで中断され
なくなり、大きな BGSAVE 下でのテール遅延が顕著に下がります(v1.25
precision:c=50、value=10 KB SET で p999 -8 %、max -18 %)。
**ツマミなし、常時有効。** AOF 自体が不要なときは `--no-aof` を
そのまま使ってください。実際にディスク I/O があるときだけ bio
スレッドは動きます。

## レプリカ / キャッシュ用途では AOF オフ

既定は `--aof`(永続化)。読み取り専用レプリカや純キャッシュ用途では
書き込みごとにディスク I/O が無駄になります:

```sh
kevy --port 6004 --no-aof
```

スループットへの影響は書き込み比率次第。**テール遅延の下がり方は
中央値より顕著**。

## カーネル `mitigations=off`(Spectre / BHB)

> **動かす前に全文を読むこと。これはセキュリティのトレードオフであり、
> タダ飯ではありません。**

Linux 6.x 以降、Spectre BHB 緩和が既定で有効です。すべての syscall が
`clear_bhb_loop`(分岐履歴バッファを掃いてユーザー / カーネル境界
越しの投機実行サイドチャネル漏洩を防ぐ小さなカーネル内ループ)を
通過します。

lx64 参照機(Intel Xeon 6、Linux 6.12)では、`clear_bhb_loop` は
kevy server の `-c1` ワークロード中で**最大の CPU 消費者** ——
**13.3%**、kevy のどのユーザー空間シンボルよりも多く食べます。`-c50`
では syscall が op に対して薄められるため 約 5% に落ちます。

### 失うもの

`mitigations=off` でブートすると、**ハードウェア脆弱性緩和を全部** 切
ります:Spectre v1/v2/BHB、Meltdown、MDS、TAA、L1TF、retbleed など
全部。**許容できる状況** は以下のみ:
- 単一テナント機(カーネルを自分で握り、信頼できないコードが動かない)
- ネットワーク L3 で隔離(または信頼できるゲートウェイの背後)
- ベンチ / テスト機

**やってはいけない場所**:マルチテナントホスト、共有 CI ランナー、
信頼できないユーザーコード(ワイヤ越しの Lua eval、第三者プラグイン
読み込みなど)を扱う機械。

### 適用方法

ブートローダのカーネル cmdline(例: `/etc/default/grub` の
`GRUB_CMDLINE_LINUX_DEFAULT`)に `mitigations=off` を足して再生成:

```sh
# Debian / Ubuntu
sudo update-grub
sudo reboot
```

再起動後に確認:

```sh
cat /proc/cmdline | grep mitigations
# ... mitigations=off ...

cat /sys/devices/system/cpu/vulnerabilities/* | head
# ... "Vulnerable" または "Mitigation: ..." が無効化されているはず
```

### 実測効果

lx64 参照機で `mitigations=off` 適用後の予測スループット:

| ワークロード | Rust client -c1 | C `redis-benchmark` -c1 |
|--------------|-----------------|--------------------------|
| 前           | ~65 k ops/s     | ~67 k ops/s              |
| 後(予測)   | ~75 k ops/s     | ~78 k ops/s              |

(数字はカーネル / CPU 依存。AMD Zen 3+ と Intel Xeon BHB と
ARM N1/N2 ではコストが異なります。**自分のハードウェアで計測のこと**。)

## netfilter / iptables ルールをカラにする(大きいが要注意)

Linux カーネルは syscall ごとに netfilter / nftables フックを通します
—— `tcp_sendmsg`、`tcp_recvmsg`、`__dev_queue_xmit`、**loopback も含めて**。
ルール集が複雑なとき(docker、libvirt、fail2ban、ufw それぞれ 50-300
ルール)、累積オーバーヘッドは巨大です。

lx64 参照機で実測(Linux 6.12、`mitigations=off`、docker + libvirt
+ Tailscale の典型的なルール ~500 本):

| ワークロード     | ルール有り(既定)| ルール空      | Δ     |
|------------------|------------------|---------------|-------|
| C c1 SET         | 80.6 k           | **108.9 k**   | +35%  |
| C c1 GET         | 80.0 k           | **108.3 k**   | +35%  |
| Rust client c1   | ~77 k            | ~96 k         | +25%  |

`mitigations=off` よりも大きい勝ち。

### 失うもの

`iptables -F` + `nft flush ruleset` でホスト上の**すべての**ファイア
ウォール / NAT ルールが消えます。その結果:

- **docker のポートフォワーディングが壊れる**(iptables NAT に依存)
- **libvirt VM が NAT を失う**(default virbr0 → eth0 の MASQUERADE)
- **Tailscale / WireGuard** の allow-list が消える
- **ufw / fail2ban / firewalld** がバイパスされる —— インターネット
  に直接さらされているホストは入力トラフィックがフィルタされなくなる

### 許容できる場面

- 専用 kevy ホストで、ファイアウォールが AWS SG / GCP firewall /
  オンプレ境界で行われている VPC 内
- すべてのサービスが同一マシン内で UNIX socket / loopback だけで通信
  するベアメタル
- ベンチ / 開発機

### NG な場面

- ハードウェアファイアウォールなしでインターネットに直接さらされている
- マルチテナント
- docker / podman に他人のワークロードが乗っている

### 適用とロールバック

```sh
# 先にバックアップ
nft list ruleset > /tmp/nft-backup.nft
iptables-save > /tmp/iptables-backup.rules

# 空にする
nft flush ruleset
iptables -F
iptables -X

# (kevy はそのまま速くなる; 他のサービスは自分で確認)

# 必要時に戻す(例: docker を再起動する前)
iptables-restore < /tmp/iptables-backup.rules
nft -f /tmp/nft-backup.nft  # xtables-compat の警告は無害
```

より安全な代替: **ルールは残したまま kevy ポートだけ早期 ACCEPT**:

```sh
iptables -I INPUT 1 -p tcp --dport 6004 -j ACCEPT
iptables -I OUTPUT 1 -p tcp --sport 6004 -j ACCEPT
```

+35% の半分くらいを回収しつつ、ファイアウォールの姿勢は維持できます。

## Profile-guided optimization(PGO)

ワークロード固定のデプロイ(read/write 比、コマンド分布、接続数が
判っている)では PGO がランタイムプロファイルを使ってバイナリを
最適化できます。lx64 で 1-10% を実測; `drain_inbound` とディスパッチ
ループで最も大きい。

```sh
# Step 1: 計装ビルド
RUSTFLAGS="-Cprofile-generate=/tmp/pgo" cargo build --release

# Step 2: 代表的ワークロードを走らせて profile 収集
LLVM_PROFILE_FILE=/tmp/pgo/kevy-%m_%p.profraw \
  ./target/release/kevy --port 6004 --no-aof &
# 別シェルで実本番形状のワークロードを ~30 秒
kill %1
sleep 3  # profile data flush

# Step 3: merge
llvm_profdata=$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-profdata
$llvm_profdata merge -o /tmp/pgo/merged.profdata /tmp/pgo/*.profraw

# Step 4: 再ビルド
cargo clean
RUSTFLAGS="-Cprofile-use=/tmp/pgo/merged.profdata" cargo build --release
```

`llvm-profdata` を取るには `rustup component add llvm-tools-preview`
が必要。merged.profdata は約 70 KB で、ワークロード形状が変わらない
限り同じ profile を使い回せます。

PGO はアップストリームのリリースには**入れていません** —— ワーク
ロードに紐づくため。1-10% を気にしないユーザーが大多数; 気にするデプロイ
は上記レシピで自分で焼いてください。

## `io_uring` SQPOLL — 実測で却下

カーネルが専用スレッドで io_uring 投入キューを polling —— op 毎の
`io_uring_enter` syscall を消します。

ワイヤレベルの実装は `kevy_uring::IoUring::new_sqpoll` にありますが
**シャード reactor には接続していません**。kevy の thread-per-core
配置との組み合わせは推奨しません。ring 1 つに付きカーネル poll
スレッドが 1 つ立つので、N シャード = N 個の 100% スピンする
カーネルスレッドが シャードスレッドと同じコアを取り合います。
lx64 参照機(10 シャード / 16 コア)で -c1 と -c50 ともに
**2–15× 回帰** を実測しました。

SQPOLL は単一スレッド reactor + poll スレッド用の余剰コアがある
構成に向いた設計です。kevy の per-core 設計はすでに CPU を使い
切っているため、カーネル poll スレッドを足すと CPU が半分になり
ます。詳細は `bench/PERF-ATTACK-LOG-2026-06-20.md` の D5。

## もう効かないこと

- `taskset` で単一コアに絞る: io_uring が並列性を失い、shared-nothing
  シャード配置のほうが速い
- THP を無効化: kevy のアロケータパターンに目に見える効果なし
- `numactl --interleave`: 多 socket でしか意味なし。lx64 は単一 socket
- slowlog を無効化: 既定でオフ(`slower-than-micros = -1`)

## 関連

- [`bench/PERF-PROFILE-2026-06-20.md`](../../bench/PERF-PROFILE-2026-06-20.md) —— このツマミ一覧を導いたフレームグラフ診断
- [`bench/PERF-ATTACK-LOG-2026-06-20.md`](../../bench/PERF-ATTACK-LOG-2026-06-20.md) —— ツマミ毎の実測ログ
- [`bench/REPORT.md`](../../bench/REPORT.md) —— ベンチ方法論

# kevy のチューニング

kevy の op あたりのコストを変えるランタイムノブのリファレンスです — CPU レイアウト、リアクター選択、永続化、メモリ上限、ネットワークトランスポート、そして Linux 側のいくつかのレバー。

## このドキュメントが必要になるとき

参照すべきとき:

- ベンチマークが kevy をスループットや遅延のターゲット以下に出していて、次にどのノブを回すか知りたい。
- デフォルト(TCP ループバック、io_uring 自動検出、`appendfsync everysec`、`maxmemory` なし)がワークロードに合わないホストに kevy をデプロイする — 例えばスパースコネクションのサービス、NVMe バックドの耐久性要件、メモリ上限付きのキャッシュティア。
- `perf` で kevy をプロファイルしていて、デバッグ行テーブルを残すビルドプロファイルが必要。

ノート PC で kevy を立ち上げて数字が良ければ、このページは不要です。デフォルトは各ワークロードで適度に良くなるよう調整されています。

## 中心となる考え方

kevy はスレッド・パー・コアのサーバーです。OS スレッドごとに 1 シャード、CRC16 ハッシュタグで分割した共有なしのキー空間、各シャードに busy-poll リアクターがあります。デフォルトは「各ワークロードでまあまあ」を狙います。チューニングとは、**実際のパフォーマンスデータがボトルネックだと示すもの**に合わせてシャード数、リアクター、永続化ポリシーをマッチさせることです。先回りでノブを切り替えないでください。計測し、コストを特定し、変数を 1 つずつ変えてください。

## チューニングプレイブック

### CPU とシャード

| ノブ | どこ | デフォルト | 効果 |
|------|-------|---------|--------|
| `--threads N` / `KEVY_THREADS` | CLI / env | オンラインコア数 | シャード数。OS スレッドごとに 1 シャード |
| `--accept-shards K` | CLI | 全シャードが accept | 最初の K シャードだけがリスナーを bind、残りは compute-only |
| CPU pinning | `taskset` / `numactl` | なし | シャードを固定のコア集合にロック |

**`--threads` の選択。** ワークロードに実在する並列性に設定してください。シングルクライアントのパイプラインベンチ(`-c 1 -P 16`)は 1 シャードを飽和させます。ここで `--threads 10` にすると 9 つのシャードが仕事のない busy-poll を走らせ、シャード 0 のキャッシュラインを奪います。本物のマルチクライアントなら、`min(cores, expected concurrent clients / 4)` から始めて計測してください。

**`--accept-shards` の選択。** コネクション対シャードの比率が低い(スパースコネクションのワークロード — 例 50 クライアントを 10 シャードで = 5 conns/shard)とき、イテレーションごとの busy-poll オーバーヘッドが分摊らなくなりスループットが落ちます。経験則は `ceil(conns / 20)` — 50 conns なら `--accept-shards 3` にし、3 つの listen シャードがそれぞれおよそ 17 接続を取り、残りのシャードは compute-only ですが内部ディスパッチャ経由でクロスシャード仕事を受け続けます。経験的なスイートスポットは点推定より広いです。フルスイープと、クロスシャードホップ税が accept 集中の利得を上回るケースの議論は [docs/accept-shards.md](https://github.com/goliajp/kevy/blob/develop/docs/accept-shards.md) を参照。

**CPU pinning。** ベンチや単一テナントのホストでは、kevy を固定コア集合に pin すると NIC IRQ → softirq → ユーザースレッドのパスを同じ L1/L2 に保てます:

```sh
taskset -c 0-9 kevy --port 6004 --threads 10
```

同じマシン上でクライアントが動くなら、サーバーとクライアントを**互いに素**なコアレンジに pin します(サーバー `0-9`、クライアント `10-15`)。共有コアは reactor の利得を圧倒するスケジューラ ping-pong を再導入します。

### リアクター選択

| プラットフォーム | デフォルト | 上書き |
|----------|---------|----------|
| Linux ≥ 5.19 | io_uring(自動検出) | `KEVY_IO_URING=0` で epoll に強制、`KEVY_IO_URING=1` で io_uring 必須、seccomp で `io_uring_setup` がブロックされていれば大きく終了 |
| macOS / *BSD | kqueue | 設定不可 |
| 旧 Linux | epoll | n/a |

Linux の自動検出は起動時に `io_uring_setup` を走らせます。syscall がブロック(seccomp プロファイル、ロックダウンされたコンテナ)されていれば kevy は無言で epoll にフォールバックします。無言で劣化させず大きく失敗させたい硬化デプロイでは `KEVY_IO_URING=1` を設定し、io_uring が本当に使えなければサーバーが起動を拒否するようにしてください。逆に、再現可能な epoll vs io_uring ベンチや、カーネルリグレッション回避のために io_uring を外したい場合は `KEVY_IO_URING=0` を設定します。

```sh
KEVY_IO_URING=1 kevy --port 6004   # io_uring 必須、ブロックされていれば終了
KEVY_IO_URING=0 kevy --port 6004   # epoll 強制
```

### 永続化

AOF ポリシーは `appendfsync`(config ファイルまたは `CONFIG SET`)で制御します。3 つの値は Redis セマンティクスに一致します:

| `appendfsync` | 耐久性 | コスト |
|---------------|------------|------|
| `always` | 各書き込みは応答前に `fsync` | 最高遅延。NVMe sync 遅延で律速 |
| `everysec`(デフォルト) | バックグラウンドスレッドで毎秒 `fsync` | データロス窓は最大 1 秒。ホットパスコストはほぼゼロ |
| `no` | `fsync` しない。カーネルが自分のスケジュールで flush | 最速。データロス窓はページキャッシュ flush 間隔 |

`everysec` のバックグラウンド `fsync` はシャードのホットパスから外れた専用 bio スレッドで走るので、シャードのテール遅延はディスク遅延に結合しません。純粋なキャッシュや読み取りレプリカでは、`--no-aof` で AOF を完全に無効にする(AOF ファイルがまったく書かれず、バッファにも入らない)選択もあります。

### メモリ

| ノブ | デフォルト | やること |
|------|---------|--------------|
| `maxmemory` | 無制限 | バイト単位のハードメモリ上限。達するとエビクションポリシーが動く |
| `maxmemory-policy` | `noeviction` | 上限に達したときどのキーを落とすか |
| `maxmemory-samples` | 5 | 近似 LRU/LFU ポリシーのサンプルサイズ |

エビクションポリシーは Redis をミラーします: `noeviction`、`allkeys-lru`、`allkeys-lfu`、`allkeys-random`、`volatile-lru`、`volatile-lfu`、`volatile-random`、`volatile-ttl`。`noeviction` は上限に達すると書き込みを OOM で失敗させ、プライマリストアの安全な既定です。`allkeys-*` 系は任意のキーが使い捨てなキャッシュ層で正しい選択です。

`maxmemory-samples` は近似ポリシーにとって品質 vs コストのダイヤルです — サンプルキーを増やすほど真の LRU/LFU により近い近似が、エビクションごとの CPU コストと引き換えに得られます。既定の 5 はほとんどのキャッシュワークロードで十分です。アクセスパターンでエビクションが悪い犠牲者を選んでいるのが見えるなら 10 に上げ、エビクション自身がプロファイルに出てくるときだけ 3 に下げてください。

### ネットワーク

デフォルトのトランスポートは TCP です。クライアントが同一ホストに居るなら、Unix-domain ソケットに切り替えてループバック TCP スタックを完全にスキップします:

```sh
KEVY_UNIX_SOCKET=/tmp/kevy.sock kevy --port 6004
redis-cli -s /tmp/kevy.sock SET foo bar
```

サーバーは dual-bind します: TCP はリモートクライアント用に上がったまま、UDS がローカルを扱います。RESP セマンティクスは同じ、シャードランタイムも同じ。ローカルクライアントのワークロードでは利得が大きいです(小ペイロードサイズではループバック TCP パスが支配コスト)。詳細な数値、権限モデル、UDS が当てはまらないケースは [docs/uds.md](uds.md) を参照。

**bind アドレスの警告。** kevy には今日 AUTH も TLS もありません。非ループバックアドレス(`--bind 0.0.0.0` または任意の公開インタフェース)に bind すると起動警告が出ます。ネットワーク上の誰でもコマンドを発行できてしまうためです。kevy はプライベートネットワーク境界、または認証を終端させるプロキシの背後で動かしてください。

### Linux カーネルノブ

ホストレベルのレバーが 2 つあり、kevy の下にあるカーネルフロアを動かします。両方ベンチ/単一テナント専用です — 適用前にトレードオフを読んでください。

**Spectre / BHB ミティゲーション。** ミティゲーションが有効(デフォルト)の Linux 6.x カーネルでは、全 syscall が `clear_bhb_loop` 等の代金を払います。小ペイロード `-c 1` ワークロードではこれが kevy run で単独最大の CPU 消費です。カーネルコマンドラインで無効化:

```sh
# GRUB_CMDLINE_LINUX_DEFAULT に `mitigations=off` を追加、その後:
sudo update-grub && sudo reboot
cat /proc/cmdline | grep mitigations
```

は untrusted コードが走らない単一テナントマシンでだけ受け入れられます(ワイヤから来る Lua なし、サードパーティプラグインなし、マルチテナントコンテナなし)。マルチテナントホスト、共有 CI ランナー、untrusted ユーザーコードを処理する任意のものには適用しないでください。利得は `-c 1` で +10–15% 範囲、ワークロードがパイプライニングするほど縮みます。

**`.text` セグメントのヒュージページ。** kevy は自分のコードセグメントに `madvise(MADV_HUGEPAGE)` を呼べます。これでカーネルが kevy バイナリの命令を 4 KiB ではなく 2 MiB ページで裏付けます。利得はホット dispatch ループの iTLB フットプリントが小さくなることです。ランタイムコストは事実上ゼロで、`/sys/kernel/mm/transparent_hugepage/enabled` が `always` または `madvise` の Linux ホストでは有効にする価値があります。トレードオフは起動時の `madvise` 呼び出し 1 回分の小さなコストだけです。`mitigations=off` とは違ってセキュリティのトレードオフはありません。

## プロファイリング

実シンボルに解決される `perf record` フレームグラフのためには、`release-perf` プロファイルでビルドしてください — 最適化レベルは `release` と同じで、デバッグ行テーブルが残ります:

```sh
cargo build --profile release-perf
./target/release-perf/kevy --port 6004 --threads 1 &
KEVY_PID=$!

perf record -F 999 -p $KEVY_PID -g --call-graph=fp -- sleep 30
perf report --stdio | head -60

# インライン展開シンボルの生アドレスを解決:
addr2line -e ./target/release-perf/kevy -f -i 0x<addr>
```

標準 `release` プロファイルは行テーブルを strip するので、`perf` は生アドレスを返してシンボルなし、`addr2line` は `??` を返します。`release` バイナリをプロファイルしないでください。まず `release-perf` でリビルドを。

`clear_bhb_loop` 等のカーネル側コストをシンボル単位で帰属させるには、`fp` の代わりに `--call-graph=dwarf` でキャプチャし、同じ `addr2line` フローを使ってください。dwarf アンワインダは遅いですが syscall 境界を越えて正しく展開します。

## トレードオフ

| ノブ | コスト | 買えるもの |
|------|-------|------|
| `--threads N`(上げる) | N > ワークロード並列なら遊休 busy-poll シャードに無駄な CPU | 同時クライアント容量増 |
| `--threads N`(下げる) | クロスシャードホップ税 1 シャード分回避 | スパースコネクションでの無駄 CPU 減 |
| `--accept-shards K` | リスナー集中。クライアントが生 `connect` するならエントリポイントが減る | accept する各シャード上で iter ごとのオーバーヘッドが多コネクション間で分摊 |
| `KEVY_IO_URING=1`(強制) | seccomp で io_uring がブロックされていればサーバー起動拒否 | 硬化ホストでの epoll への無言劣化なし |
| `KEVY_IO_URING=0`(epoll 強制) | io_uring の op あたり節約を諦める | 再現可能な epoll ベースライン。カーネルリグレッション回避 |
| `appendfsync always` | 全書き込みが `fsync` でブロック | データロスゼロの耐久性 |
| `appendfsync no` | データロス窓 = ページキャッシュ flush 間隔 | 最速の書き込みパス |
| `--no-aof` | 永続化まったくなし | ディスク I/O 最小化。レプリカ/キャッシュ用途 |
| `maxmemory` 設定 | 書き込みが失敗(`noeviction`)またはエビクト(`allkeys-*`)し得る | メモリフットプリント有界化 |
| `maxmemory-samples` 上げる | エビクションごとの CPU コスト | 近似 LRU/LFU の犠牲者選択が改善 |
| Unix-domain socket | ローカル限定。ファイルシステム権限のセキュリティモデル | TCP ループバックスタックをスキップ |
| `mitigations=off` | Spectre / Meltdown / MDS 等のミティゲーション全 off | syscall パス税を取り戻す |
| `.text` への `MADV_HUGEPAGE` | 意味のあるコストなし | dispatch ループの iTLB フットプリント縮小 |
| `release-perf` ビルド | バイナリが大きくなる(デバッグ行テーブル) | `perf` がシンボル解決 |

## FAQ

**`--accept-shards` は常に設定すべきですか?**

いいえ。このノブは conns/shards が低く busy-poll body が分摊できないスパースコネクション用です。デンスコネクション(例 1000 クライアント × 10 シャード = 100 conns/shard)ではデフォルト — 全シャードが accept — が正しいです。リスナーを均等に広げると accept 側競合が減るからです。`ceil(conns / 20)` は実際にスパースコネクションのケースになっているときだけ適用してください。

**io_uring は常に epoll より速いですか?**

Linux ≥ 5.19 で投入をバッチするワークロードでは、はい、目に見えてです。古いカーネル、`io_uring_setup` をブロックする seccomp フィルタ、バッチ機会のない op ごと 1 syscall に支配されたワークロードでは差が縮みます。自動検出が正しいデフォルトです。実測の理由か、無言にフォールバックせず大きく失敗させるべき硬化デプロイがある場合だけ上書きしてください。

**`appendfsync` の本番スイートスポットは?**

ほぼ全員 `everysec`。データロスを 1 秒に有界化し、`fsync` をホットパスから外し、テール遅延への影響はほぼゼロです。本当にゼロデータロスが必要な耐久性ストーリーがあるときだけ `always`(その時 NVMe `fsync` 遅延がテール遅延を律速します)。AOF がウォームリスタートのためだけに存在する純粋キャッシュにのみ `no`。

**`MADV_HUGEPAGE` はいつ必要ですか?**

`perf` がホット dispatch ループ上で iTLB ミスを示すとき、あるいはホストの `/sys/kernel/mm/transparent_hugepage/enabled` が `madvise` のとき(その場合は他に kevy を opt-in させるものがありません)。THP がそもそも有効な Linux ホストではコストなしのノブなので、デフォルト姿勢は「on のままにしておく」です。macOS / BSD には等価物がありません。

**`perf` レポートが生アドレスばかりです。何を間違えましたか?**

`cargo build --release` バイナリをプロファイルしました。標準 release プロファイルはデバッグ行テーブルを strip するので、`perf` と `addr2line` には解決対象がありません。`cargo build --profile release-perf` でリビルドして再記録してください。

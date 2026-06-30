# 永続化

kevy が再起動をまたいでデータを保持する仕組み — AOF、スナップショット、fsync ポリシー、リライト/コンパクション、クラッシュリカバリ、そしてそれらすべてを観測するためのイントロスペクションについて説明します。

## このドキュメントが必要になるとき

次のような場面で参照してください。

- 本番デプロイの耐久性ポリシー(ゼロロス vs スループット)を選定するとき。
- 書き込み中心ワークロードに対するディスク使用量とリプレイ時間の見積もりをするとき。
- 想定外のオンディスク成果物 — quarantine ファイル、古い `.rewrite` 一時ファイル、`.premigration.*` バックアップ — をデバッグするとき。
- ホストアプリケーションに `kevy_embedded::Store` を組み込んでいて、プロセスクラッシュで何が生き残り、何が失われ、ホストの中からそれをどう観測できるかを知りたいとき。
- 再起動をまたいで TTL の挙動がおかしいキーを調べているとき。

「`kill -9` で生き残るか?」の手短かな答えだけ欲しいなら、答えは「はい。デフォルトポリシーで最大 1 秒分の書き込みが失われるだけ」です。

## 中心となる考え方

各シャードは永続化ディレクトリに 2 つのファイルを所有します。変更コマンドの追記専用ログ(`aof-<id>.aof`)と、オプションのバイナリスナップショット(`dump-<id>.rdb`)です。AOF 単独で完全な永続記録になります。スナップショットはリプレイ時間を抑えるためにだけ存在します。起動時に kevy はスナップショットがあればロードし、その後 AOF をリプレイします。スナップショットが成功すると AOF はリセットされ、2 つのファイルが履歴全体をちょうど一度ずつカバーするようになります。

## 動かしてみる例

### サーバーモード

これを `kevy.toml` に置き、`kevy --config kevy.toml` で起動します。

```toml
# kevy.toml
dir         = "/var/lib/kevy"
port        = 6379
threads     = 4
appendonly  = true

# AOF 耐久性 — 全ノブは下の表を参照。
appendfsync                 = "everysec"   # always | everysec | no
auto_aof_rewrite_percentage = 100          # 前回リライト時から AOF が倍増したらリライト
auto_aof_rewrite_min_size   = 67108864     # …かつ少なくとも 64 MiB
```

通常の Redis スタイルのコマンドを RESP 経由で操作できます。

```text
$ redis-cli -p 6379 BGSAVE
Background saving started

$ redis-cli -p 6379 BGREWRITEAOF
Background append only file rewriting started

$ redis-cli -p 6379 INFO persistence
aof_enabled:1
appendfsync:everysec
aof_rewrite_in_progress:0
aof_rewrites_total:3
```

`CONFIG SET appendfsync always` で、再起動なしにポリシーを生で再チューニングできます。

### 組み込みモード

`Cargo.toml` にクレートを追加します。

```toml
[dependencies]
kevy-embedded = "*"
```

そして `main.rs`:

```rust
use std::time::Duration;
use kevy_embedded::{AppendFsync, Config, KevyMetric, Store};

fn main() -> std::io::Result<()> {
    let cfg = Config::default()
        .with_persist("/var/lib/myapp/kevy")
        .with_appendfsync(AppendFsync::EverySec)
        .with_auto_aof_rewrite(100, 64 * 1024 * 1024)
        .with_metric_sink(|m| match m {
            KevyMetric::Replay { commands, bytes, elapsed_ms } => {
                eprintln!("kevy replay: {commands} cmds / {bytes} B in {elapsed_ms} ms");
            }
            KevyMetric::Rewrite { keys, before_bytes, after_bytes, elapsed_ms } => {
                eprintln!(
                    "kevy rewrite: {keys} keys, {before_bytes} -> {after_bytes} B in {elapsed_ms} ms"
                );
            }
            _ => {}
        });

    let store = Store::open(cfg)?;

    store.set("hello", b"world")?;
    store.pexpire("hello", Duration::from_secs(300))?;

    // ある時点のスナップショット。ファイルがディスクに書き出された後に戻る。
    // シャードごとのロックはビューの freeze と最後の rename のあいだだけ保持される。
    store.save_snapshot()?;

    // オンデマンドの AOF コンパクション。ロック規律は save_snapshot と同じ。
    let _stats = store.rewrite_aof()?;

    // ライブイントロスペクション。
    let info = store.info();
    println!("{} keys, {} bytes AOF", info.keys, info.aof_bytes);

    Ok(())
}
```

デフォルト設定で新規に作った組み込みストアは AOF だけを書きます。`save_snapshot` を呼ぶまでスナップショットファイルは現れません。これは想定どおりで、AOF 単独でキー空間を再構築できます。

## 設定ノブ

### 耐久性と AOF 成長

| ノブ | サーバー(TOML / `CONFIG SET`) | 組み込み(`Config::…`) | デフォルト | 備考 |
|---|---|---|---|---|
| AOF fsync ポリシー | `appendfsync`(`always` / `everysec` / `no`) | `with_appendfsync(AppendFsync::…)` | `EverySec` | サーバーでは生で変更可能。 |
| AOF 有効化 | `appendonly`(`true` / `false`) | `with_persist(...)` で暗黙的に | サーバーは `true`、組み込みは `with_persist` まで off | 無効化するとオンディスク永続化を全部スキップ。 |
| 自動リライトのパーセンテージ | `auto_aof_rewrite_percentage` | `with_auto_aof_rewrite(pct, min)` の第 1 引数 | `100` | `0` で自動リライト無効。 |
| 自動リライトの最小サイズ | `auto_aof_rewrite_min_size` | `with_auto_aof_rewrite(pct, min)` の第 2 引数 | `67108864`(64 MiB) | 両方の閾値を満たしたときだけ自動リライトが発火。 |
| 永続化ディレクトリ | `dir` / 環境変数 `KEVY_DIR` | `with_persist(path)` | サーバーは `./data`、組み込みは none | kevy インスタンスごとに 1 ディレクトリ。 |
| リアクター/リーパー周期 | reactor tick、約 100 ms | バックグラウンドリーパー、または `Store::tick` 呼び出し | 約 100 ms | `EverySec` flush、自動リライトチェック、TTL eviction を駆動。 |

### トリガー一覧

| 動作 | サーバー | 組み込み | ブロッキング形状 |
|---|---|---|---|
| 同期スナップショット | `SAVE` | `Store::save_snapshot()` | ファイルがディスクに書き出された後に戻る。ロックは freeze + rename のあいだだけ保持。 |
| バックグラウンドスナップショット | `BGSAVE` | ワーカースレッドから `save_snapshot` を呼び出す | 即時に戻る。コミットはディスク書き出しが終わってから 1 reactor tick 以内に確定。 |
| AOF リライト | `BGREWRITEAOF` | `Store::rewrite_aof()` | アトミックな rename の後に戻る。シリアライズはキー空間がライブのまま走る。 |
| fsync を生で再チューニング | `CONFIG SET appendfsync everysec` | `Config` を再構築 | n/a |

### fsync ポリシーの意味

| ポリシー | 耐久性 | コスト |
|---|---|---|
| `Always` | ゼロロス — 各書き込みは応答前に fsync | スループット約 50% |
| `EverySec`(デフォルト) | クラッシュで最大約 1 秒分の書き込みが失われる可能性 | 安い |
| `No` | OS のページキャッシュ flush に委ねる | 最安 |

## トレードオフと限界

**ポリシー別のスループット vs データロス。** `Always` は各返信を `fsync` でブロックします。`kill -9` でコマンドロスゼロを保証する唯一のポリシーで、SET 中心ワークロードのスループットを一般的な NVMe で約半分にします。`EverySec` は 1 秒ごとにバックグラウンドで flush し、クラッシュ時にその窓ぶんを失う可能性があります — デフォルトなのはちょうど Redis のトレードと一致し、失う窓が通常許容範囲だからです。`No` はカーネルに判断を委ねます。スループットは最大ですが、クラッシュ時にページキャッシュ内のデータがすべて失われる可能性があり、それは数秒分にもなり得ます。

**AOF リプレイコスト vs スナップショットロードコスト。** スナップショットがない場合、起動時間は AOF のバイト数に対し線形に伸びます。4 GiB の AOF はローカル NVMe で数秒、40 GiB なら 1 分以上です。スナップショットはそれを上限化します — ロードは 1 回のストリーミング読み出しと、スナップショット後の AOF 短い末尾だけ — が、一時的なビュー freeze(O(keys)、コレクション値は refcount 共有なのでキーあたりナノ秒)と、スナップショット中に最初に変更されたコレクションの 1 回コピーがコストになります。書き込み中心のワークロードでは、定期的な `BGSAVE` よりも自動リライトに任せて AOF を有界に保つほうを優先してください。リライトは管理対象を 1 ファイルに保ったまま、同じ起動時間の上限を与えてくれます。

**バックグラウンドジョブの並行性。** 各シャードは一度に最大 1 つだけバックグラウンド save または rewrite を走らせます。ジョブ実行中に届いた重複リクエストはログ行とともにスキップされ、キューには積まれません。

**TTL の永続化。** TTL は絶対 Unix ミリ秒のデッドラインとして書かれます(AOF では `PEXPIREAT`、スナップショット形式では絶対値フィールド)。何回再起動しても元の期限の瞬間が保たれ、プロセスがダウンしていた時間も正しく差し引かれます。相対残り時間を記録した古い AOF も読み込み可能(エントリ時点で相対として扱う)で、新しい書き込みは常に絶対です。`EXPIREAT` と `PEXPIREAT` はクライアントコマンドとして公開されています。

**シャードレイアウトの変更はクラッシュ idempotent。** `--threads` / `shards` を変更すると `.reshard` 一時名で新しいスナップショットを書き、耐久性のある `reshard.journal` 経由でコミットし、中断された移行は次回起動時にロールフォワードされます。元ファイルは `.premigration.<unix_ts>` バックアップとして残ります。ジャーナルはコミットポイントなので、手で削除してはいけません。

**永続化されないもの。** Pub/sub のチャネル、サブスクリプション、未配信メッセージはメモリにしか存在しません。`BLPOP` や blocking `XREAD` のようなブロッキングコマンドの待機はコネクション状態であってデータではありません。これらは AOF にもスナップショットにも書かれず、リプレイされません。

## FAQ

### AOF ファイルが大きくなっています — どうコンパクトしますか?

サーバーでは `BGREWRITEAOF`、組み込みモードでは `Store::rewrite_aof()` を実行します。リライトは現在のキー空間を再構築する最小コマンド集合 — キーごとに 1 つの `SET` / `HSET` / 等、TTL 付きキーには `PEXPIREAT` — としてログを作り直し、新ファイルをアトミックに差し替えます。`hot` への 1 万回の上書きは `SET hot <latest>` 1 つに集約されます。

無人運用なら自動リライトをデフォルト — 前回リライトサイズから 100% 成長、最低 64 MiB — のままにしておけば、リアクターが勝手にコンパクションを発火します。`auto_aof_rewrite_percentage = 0` にすると無効化され、リライトは完全に手動になります。

リライトはキー空間にとってノンブロッキングです。シリアライズと `fsync` は読み書きが流れたまま走り、リライト中に着地した書き込みは diff バッファに tee され、コンパクトされたイメージに追記されます。リライトが途中でクラッシュしても元の AOF は無傷で(差し替えはアトミックな `rename`)、残った `aof-<id>.aof.rewrite` 一時ファイルは削除しても安全です。

### 永続化を完全に無効化できますか?

はい。2 つの方法があります。

- **サーバー:** `kevy.toml` で `appendonly = false`(または `--dir` を省略)。サーバーは純粋なインメモリキャッシュとして動き、`aof-*` も `dump-*` も作られません。
- **組み込み:** `with_persist(...)` を呼ばずに `Config` を構築。`Store::open` は完全にメモリ内でキー空間を運用し、`save_snapshot` と `rewrite_aof` は API 上では no-op になります(あるいは永続化ディレクトリ未設定のエラーが返ります)。

永続化はしたいがスナップショット間で AOF をまったく成長させたくない、という組み合わせはサポートされていません。kevy の耐久性モデルは AOF-first で、スナップショットは AOF リプレイを上限化するために存在し、AOF を置き換えるためではありません。

### 高書き込み負荷中のスナップショットコストは?

ブロッキング部分は微小です。シャードごとの keyspace freeze は O(keys) で、O(bytes) ではありません。コレクション値は参照カウントされ、ライブストアと共有されているからです。100 万キーのシャードでも freeze は 1 桁ミリ秒で済みます。シリアライズ自身はキー空間がライブのまま走ります — 書き込みは止まりません。

一時的に払うコストはメモリです。スナップショット書き出し中に変更されたコレクション(list、hash、set、sorted-set)は 1 度だけクローンされ、ライブストアは freeze されたビューを乱さずに先へ進めます。普通の文字列キーへの `SET` 中心ワークロードでは追加メモリはほぼ無視できます。少数の巨大コレクションへの `HSET` / `LPUSH` 中心ワークロードでは、それらの特定コレクションの常駐サイズが一時的に倍になる可能性があります。

成功したスナップショットは AOF もリセットします。ログがこれまで持っていた内容はスナップショットに移り、ログは freeze 後に着地した書き込みだけで再開されます。再起動時は履歴を二重適用することなく snapshot + log をロードします。

### 次回起動時のリカバリはどう順序づけられますか?

各シャードについて、順番に:

1. **スナップショットをロード。** `dump-<id>.rdb` があればキー空間にストリーミング。ロード中に期限切れ TTL は破棄。
2. **AOF をリプレイ。** `aof-<id>.aof` を先頭から読み、各フレームを適用。
3. **末尾を処理。** クリーンなファイルはそのまま全適用。途中で切れた末尾(クラッシュ途中の追記)は途中フレームを捨てて前置部分を適用。破損フレームは将来の起動をブロックしないよう不正バイトを `aof-<id>.aof.panic-quarantine.<unix_ts>` に退避し、その後前置部分を適用。隔離された末尾は二度と再適用されません。そこから何か復旧する必要があれば手で調査してください。
4. **壁時計時間付きの 1 行サマリーをログ出力**:

   ```text
   kevy: AOF /data/kevy/aof-0.aof replayed 145313 commands from 418261733 bytes in 247 ms (clean)
   ```

5. **中断されたシャードレイアウト移行があれば** `reshard.journal` をリプレイしてロールフォワード。

リプレイ時間の行をウォッチし、自動リライトでそれを有界に保ってください — リプレイ時間はリライトされていない AOF サイズに線形に伸びます。

### 組み込みホストプロセスの中から永続化を監視するには?

2 つの面があります。

**ポーリング。** `store.info()` は `keys`、`used_memory`、`aof_bytes`、`expire_pending`、`evictions`、`expired_keys` を持つ `KevyInfo` 構造体を返します。同じ情報を細かく扱うヘルパーもあります。

```rust
store.dbsize();                 // ライブキー数
store.ttl(key);                 // Option<Duration>(None = キーなし / TTL なし)
store.ttl_ms(key);              // Redis PTTL セマンティクス: -2 キーなし、-1 TTL なし、その他 ms
store.expire_pending_count();   // TTL を持つライブキー数
store.used_memory();            // 常駐バイトの推定
store.expired_keys_total();     // 合計期限切れ数(lazy + リーパー)
store.evictions_total();        // maxmemory による合計 eviction 数
```

TTL があるはずなのに `expire_pending_count() == 0` なら、それは TTL サブシステムがキーを登録しなかった古典的な兆候です。

**プッシュ。** `Config::with_metric_sink(...)` を登録すると、AOF リプレイ(起動時)と各 AOF リライト(コンパクション)で `KevyMetric` イベントが届きます。シンクは発行スレッド(バックグラウンドリライトならリーパー)で同期実行されるので、コールバックは速く保ってください。`KevyMetric` は `#[non_exhaustive]` です — 常に `_` アームをマッチして将来互換を保ちましょう。

### 永続化ディレクトリの中の各ファイルは何ですか?

| パターン | 意味 |
|---|---|
| `aof-<id>.aof` | シャード `<id>` のライブ AOF。 |
| `dump-<id>.rdb` | シャード `<id>` のバイナリスナップショット。 |
| `shards.meta` | 記録されたシャード数とルーティング方式。 |
| `dump-<id>.rdb.tmp` | 進行中のスナップショット書き込み。古ければ削除して安全。 |
| `aof-<id>.aof.rewrite` | 進行中の AOF リライト/リセット。古ければ削除して安全。 |
| `dump-<id>.rdb.reshard` + `reshard.journal` | 進行中のシャードレイアウト移行。次回起動でロールフォワード。ジャーナルは絶対に手で消さない。 |
| `*.premigration.<unix_ts>` | 移行前のソースバックアップ。ロールバック用に保持。 |
| `aof-<id>.aof.panic-quarantine.<unix_ts>` | リカバリ中に退避された破損 AOF 末尾。何か救出するなら手で調査。kevy が再適用することはない。 |

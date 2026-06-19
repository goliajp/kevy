# 永続化

kevy が再起動を超えてデータを保持する方法:AOF、バイナリ・スナップ
ショット、TTL セマンティクス、AOF リライト/コンパクション、クラッシュ・
リカバリ、そしてそれらを観察するための introspection。ネットワーク・
サーバー(`kevy` バイナリ)とプロセス内組込みモード
(`kevy_embedded::Store`)の両方に適用、違いがあれば明示します。

## 2 つのオンディスク・アーティファクト

永続化は 1 つのディレクトリに置かれます(サーバー:`--dir` /
`KEVY_DIR`;組込み:`Config::with_persist(dir)`)。各 shard は自分の
ファイルを所有し、shard id でサフィックスされます:

| ファイル | 内容 | 書き込み元 |
|---------|------|----------|
| `aof-<id>.aof` | すべての変更コマンドの append-only ログ(RESP フレーム、`KEVYAOF1` magic プレフィックス) | 書き込み発生時に継続的に |
| `dump-<id>.rdb` | バイナリ point-in-time スナップショット(magic `KEVYSNAP`) | 明示的な `SAVE` / `BGSAVE`(サーバー)または `Store::save_snapshot`(組込み)時のみ |
| `aof-<id>.aof.panic-quarantine.<unix_ts>` | リカバリ中に脇に避けた壊れた AOF テール | 起動時、AOF テールが parse できないとき |

デフォルト設定の新しい組込みストアは **AOF のみ** で永続化します ——
`save_snapshot` を呼ばない限りスナップショット・ファイルは作られません。
これは期待される動作で、欠けた機能ではありません:AOF だけで完全な持
続的記録になります。

## スナップショット:`SAVE`、`BGSAVE`、`Store::save_snapshot`

成功したスナップショットは **AOF をリセット** します:スナップショッ
ト・ポイントの状態をスナップショットが運び、ログはその後に着地した書
き込みだけで再スタートします —— これで再起動時に snapshot + ログが
ロードされ、履歴を二重適用することは決してありません(v1.16.0 で組込
みの `save_snapshot` も修正、以前は完全なログを残して `RPUSH` のような
非冪等コマンドを replay で重複させていました)。

- **`SAVE`**(サーバー)は同期ブロッキング持続化、Redis 契約と同じ:
  スナップショットがディスクに着いた後に返ります。あるシャードでバッ
  クグラウンド save/rewrite が実行中なら、その shard の `SAVE` はログ
  行を出してスキップされます。
- **`BGSAVE`**(サーバー、v1.16.0)は shard 毎に copy-on-write ビュー
  を凍結してすぐ返ります;バックグラウンド・スレッドがスナップショッ
  トを書き、reactor tick がスナップショットの rename とログ・リセッ
  トを 1 つの隣接 critical section にまとめてコミットします。`BGSAVE`
  発行後の書き込みは swap まで古いログに落ち続け、いつ crash しても
  何も失われません。
- **`Store::save_snapshot`**(組込み)は `SAVE` と同様に同期、同じ
  ビュー凍結トリックを使います:per-shard ロックは凍結と commit の間
  だけ保持、ディスク書き込み中は保持しません。

## fsync ポリシー(`appendfsync`)

AOF をディスクに flush する頻度を制御します。デフォルトは
**`EverySec`**(Redis と同じ)。

| ポリシー | 持久性 | コスト |
|---------|------|------|
| `Always` | 損失ゼロ(各書き込みが応答前に fsync) | スループット ~50% |
| `EverySec`(デフォルト) | crash で ≤ 1 秒の書き込み損失 | 安価 |
| `No` | OS ページキャッシュ任せ | 最安 |

サーバー側は `appendfsync` 設定キー(TOML で `always` / `everysec` /
`no`、`CONFIG SET appendfsync …` でライブ調整も可能)、組込みは
`Config::with_appendfsync(AppendFsync::Always)` で設定します。

組込みモードでは `EverySec` の flush ウィンドウはバックグラウンド TTL
reaper tick(または manual reaper モードの `Store::tick` 呼び出し)で
駆動されます。

## TTL 永続化 —— 絶対 deadline(v1.8.1+)

TTL は **絶対 Unix ミリ秒 deadline** として永続化されます(AOF の
`PEXPIREAT`、snapshot フォーマット v3 の絶対フィールド)。key は何回
再起動を経ても元の expiry インスタントを保ち、プロセスが down してい
た時間は正しく差し引かれ、deadline が既に過ぎた key はロード時に落と
されます。

> v1.8.1 以前は TTL を相対(`PEXPIRE <remaining>`)で保存し、ロード時
> に再アンカーしていたため、各再起動で key が新しいフル TTL にリセッ
> トされていました。v1.8.1 で修正(INC-2026-06-09)。古い相対
> `PEXPIRE` AOF エントリと v2 snapshot もロードされます(相対として扱
> われる)—— マイグレーション不要;新しい書き込みは絶対です。
> `EXPIREAT` / `PEXPIREAT` もクライアント・コマンドとして公開されて
> います。

## AOF リライト / コンパクション

AOF は書き込み毎に成長します、同じ key の繰り返し上書きも含めて。
**リライト** は現在の keyspace を再構成する最小コマンド集合として再
構築し(key 毎に `SET`/`HSET`/… 1 つ、TTL ありの key には `PEXPIREAT`
追加)、ライブ・ファイルを原子的に置換します —— つまり 10 000 回の
`SET hot …` が `SET hot <最新>` 1 つに圧縮されます。

**手動**(常に利用可能):

- サーバー:`BGREWRITEAOF` —— v1.16.0 でバックグラウンド化:`+OK` は
  shard が keyspace の copy-on-write ビューを凍結したら返ります
  (O(n)-shallow、key あたりナノ秒);per-shard バックグラウンド・ス
  レッドがそれをシリアライズし、ディスク書き込みが完了した次の
  reactor tick(~100 ms)以内にコンパクトなファイルが swap されます。
  完了シグナルは `INFO persistence`(後述)で監視。shard あたりインフ
  ライト 1 ジョブ;実行中に着いたリクエストはログ行を出してスキップ。
- 組込み:`Store::rewrite_aof() -> io::Result<Option<RewriteStats>>`
  —— 呼び出し側の視点では同期(原子 rename の後に返る)ですが、per-
  shard ロックはビュー凍結と最終 swap の間だけ保持されます;シリア
  ライゼーション中は並列読み書きが流れます。

**自動**(Redis スタイルの閾値):ライブ AOF が前回リライト時のサイズ
から `percentage` 増え、**かつ** 最小 `min_size` バイトに達したとき
リライトが発火します。デフォルトは **100% / 64 MiB**。

- サーバー:設定キー `auto_aof_rewrite_percentage` /
  `auto_aof_rewrite_min_size`(`CONFIG SET` でライブ調整可)、reactor
  tick で確認。
- 組込み:`Config::with_auto_aof_rewrite(pct, min_size)`、バックグラウ
  ンド reaper tick で確認、または manual reaper モードの `Store::tick`
  呼び出し時。`pct = 0` で自動オフ(手動のみ)。

すべてのリライト・パスは keyspace に対して **非ブロッキング** です
(v1.16.0):ロック(組込み)または shard スレッド(サーバー)は
copy-on-write ビューを凍結する間と完成したファイルを swap する間だけ
保持されます —— コレクション値は refcount 共有なので凍結は O(keys)
で O(bytes) ではありません。シリアライゼーション + fsync は keyspace
がライブのまま動きます;その間に着いた書き込みは diff バッファに tee
され、圧縮イメージの後に追加されるので何も失われません。一時的なコ
ストは ビュー(key あたり数十バイト)と、リライト中に最初に変更された
任意のコレクションの 1 回限りのコピーです。リライトが途中で crash し
ても元の AOF は無傷(swap は原子 `rename`)、`<aof>.rewrite` テンプは
削除可能です。

## クラッシュ・リカバリ(起動時 AOF replay)

open 時、kevy は `dump-<id>.rdb` があればロード、続いて
`aof-<id>.aof` を replay します:

- **クリーン** → すべてのコマンドが適用。
- **テール truncated**(append 中の crash) → prefix を replay;部分
  的な末尾フレームは落とされます。リカバ可能、未完了の書き込み以上の
  データ損失なし。
- **壊れたフレーム**(例えば非 kevy バイトがファイル・パスに書き込ま
  れた) → prefix を replay、悪いテールは落とされ、問題のあるバイト
  は `aof-<id>.aof.panic-quarantine.<unix_ts>` に避けられ、将来の起動
  をブロックしません。隔離されたテールは再適用 *されません*;そこから
  何か復元する必要があれば手動で inspect してください。

各 replay は wall-clock 時間を含む 1 行サマリをログ:

```
kevy: AOF /data/kevy/aof-0.aof replayed 145313 commands from 418261733 bytes in 247 ms (clean)
```

AOF は無制限なので replay 時間がそれに伴って増えます —— この数字を
監視し、auto-rewrite でそれを制限してください。

shard レイアウト移行(`--threads` / `shards` の変更、または cluster
ルーティングの切替)は crash-idempotent です:新しいスナップショット
は `.reshard` テンプ名に着地し、永続的な `reshard.journal` が commit
ポイントをマーク、中断された移行は次回起動で roll forward されます。
ソース・ファイルは `.premigration.<timestamp>` バックアップとして残り
ます。

## Introspection(サーバー)

`INFO persistence` は応答する shard のビューを報告、reactor tick
(~100 ms)毎に更新:

```
aof_enabled:1
appendfsync:everysec
aof_rewrite_in_progress:0     # バックグラウンド save/rewrite 実行中
aof_rewrites_total:3          # open 以来完了した AOF rewrite 数
```

`aof_rewrites_total` の増加(と `in_progress` の `0` への戻り)が非同
期 `BGREWRITEAOF` / `BGSAVE` の完了シグナルです。

## Introspection(組込み)

プロセス内モードには `redis-cli` 用の TCP エンドポイントがないため、
同じシグナルは `Store` ハンドルのメソッドとして提供:

```rust
store.dbsize();                 // ライブ key 数
store.info();                   // KevyInfo { keys, used_memory, aof_bytes,
                                //            expire_pending, evictions, expired_keys }
store.ttl(key);                 // Option<Duration>(None = key なし / TTL なし)
store.ttl_ms(key);              // 生の Redis PTTL:-2 key なし、-1 TTL なし、それ以外は ms
store.expire_pending_count();   // TTL を持つライブ key(expire-set サイズ)
store.used_memory();            // 常駐バイト見積もり
store.expired_keys_total();     // 総 expired(lazy + reaper)
store.evictions_total();        // maxmemory による総 evicted
```

TTL を期待していて `expire_pending_count() == 0` の場合、TTL サブシス
テムが key を登録していない兆候です。

### プッシュ式メトリクス

継続監視のため(`info()` のポーリングと対比)コールバックを登録:

```rust
let cfg = Config::default()
    .with_persist("/data/kevy")
    .with_metric_sink(|m| match m {
        KevyMetric::Replay { commands, bytes, elapsed_ms } => { /* 起動 */ }
        KevyMetric::Rewrite { keys, before_bytes, after_bytes, elapsed_ms } => { /* コンパクション */ }
        _ => {}
    });
```

sink は AOF replay(起動)と各 AOF rewrite(コンパクション)で発火し
ます。発射スレッド(バックグラウンド rewrite の場合は reaper スレッ
ド)で同期実行されるため、高速に保ってください。`KevyMetric` は
`#[non_exhaustive]` です —— `_` アームを使って前方互換を維持してくだ
さい。

## 永続化 **されない** もの

- **Pub/sub** —— チャネル、サブスクリプション、公開メッセージはインメ
  モリのみ;pub/sub に関するものは AOF にも snapshot にも書かれません。
- **ブロッキング・コマンド待機者**(`BLPOP`、ブロッキング `XREAD`)
  —— 接続状態であり、データではありません。

## ファイル命名リファレンス

| パターン | 意味 |
|---------|------|
| `aof-<id>.aof` | shard `<id>` のライブ AOF |
| `dump-<id>.rdb` | shard `<id>` のバイナリ snapshot |
| `shards.meta` | 記録された shard 数 + ルーティング・スキーム |
| `dump-<id>.rdb.tmp` | 進行中の snapshot 書き込み;古ければ削除可 |
| `aof-<id>.aof.rewrite` | 進行中の AOF リライト/リセット;古ければ削除可 |
| `dump-<id>.rdb.reshard` + `reshard.journal` | 進行中の shard レイアウト移行(次回起動で roll forward;ジャーナルは **絶対** 手動で削除しないこと) |
| `*.premigration.<unix_ts>` | マイグレーション前ソース・バックアップ |
| `aof-<id>.aof.panic-quarantine.<unix_ts>` | リカバリ中に脇に避けた壊れた AOF テール |

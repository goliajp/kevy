# エラーリプライ・カタログ

kevyが送出するすべてのワイヤレベルのエラーリプライについて、何が引き金になり、クライアントは次に何をすべきかを引ける一覧表です。

## この表の読み方

kevyのエラーはRESPのsimple-error文字列——`-<PREFIX> <message>\r\n`——として流れます。空白区切りの最初のトークンが**プレフィックス**（`ERR`、`WRONGTYPE`、`MOVED`、`CROSSSLOT`など）です。クライアントライブラリは通常これを、`.kind`／`.code`／クラス名がプレフィックスに一致するエラーバリアントや例外として表面化します。行の残りは人間向けのメッセージです。

このカタログはプレフィックスでグループ化しています。エラーを構造的に処理するなら（推奨）、プレフィックスでマッチしてください。後続のメッセージはログとオペレーター向けであって、パースするためのものではありません。

エラーはkevyのユーザー向け契約の一部です。プレフィックスの追加・改名・転用は、パターンマッチするクライアントにとって破壊的変更になります。

## コアリファレンス

| プレフィックス | 引き金 | 回復／次の一手 |
|--------|---------------|----------------------|
| `-ERR unknown command '<cmd>'` | コマンド名が未実装、または認識されない。 | READMEのコマンドカバレッジを確認し、綴りを検証する。 |
| `-ERR wrong number of arguments for '<cmd>' command` | argcがそのコマンドの受け付ける形と一致しない。 | 文書化された引数の数で再発行する。 |
| `-ERR value is not an integer or out of range` | パース可能なi64でない値へのINCR/DECR系コマンド、または数値引数が範囲外。 | `SET`でパース可能な整数に上書きするか、入力をクランプする。 |
| `-ERR no such key` | RENAME / RENAMENX / COPY / GETEX の対象キーが存在しない。 | キーを不在として扱う（必要なら`EXISTS`で事前確認）。 |
| `-ERR kevy only supports DB 0` | `SELECT N`がN ≠ 0で発行された。 | kevyにマルチDBはない。別インスタンスか名前空間付きキーを使う。 |
| `-ERR MULTI calls can not be nested` | MULTIブロックの中でさらに`MULTI`が送られた。 | 次のトランザクションを開く前に`EXEC`／`DISCARD`を待つ。 |
| `-ERR EXEC without MULTI` | 開いたトランザクションがないのに`EXEC`が送られた。 | `MULTI`と対にするか、コマンドを落とす。 |
| `-ERR DISCARD without MULTI` | 開いたトランザクションがないのに`DISCARD`が送られた。 | `MULTI`と対にするか、コマンドを落とす。 |
| `-ERR WATCH inside MULTI is not allowed` | MULTIブロックの中で`WATCH`が送られた。 | `WATCH`は`MULTI`の前に発行する。 |
| `-ERR <cmd> not allowed inside MULTI` | キュー不能なコマンド（pub/sub、`WATCH`、`HELLO`、`RENAME`）がMULTI内でキューされた。 | これらはトランザクションの外で発行する。 |
| `-ERR Protocol error` | 受信バイト列が正しいRESPでない。 | 再接続してリトライ。続くならクライアントのシリアライザを監査する。 |
| `-ERR CONFIG SET failed for '<key>': <reason>` | 未知のCONFIGフィールド、または値が範囲外。 | `CONFIG GET *`でサポートされるフィールドと現在値を見る。 |
| `-ERR CONFIG REWRITE could not write <path>: <io-error>` | 設定TOMLのパスが存在しないか書き込めない。 | `--config`のパスとファイルシステム権限を確認する。 |
| `-WRONGTYPE Operation against a key holding the wrong kind of value` | 既存の別Redis型のキーに対してコマンドを実行した。 | [WRONGTYPEの規則](#wrongtypeの規則)参照。 |
| `-EXECABORT Transaction discarded because of previous errors.` | MULTI中にキューしたコマンドが未知の動詞、または引数が少なすぎた。EXECはバッチを拒否し、何も実行しない。 | 問題のコマンドを直してから、再度`MULTI`／キュー／`EXEC`。 |
| `-MOVED <slot> <host:port>` | キーのハッシュスロットをこのノードが所有していない。 | [クラスタルーティング系リプライ](#クラスタルーティング系リプライ)参照。 |
| `-CROSSSLOT Keys in request don't hash to the same slot` | 複数キーコマンドが複数のハッシュスロットにまたがった。 | [クラスタルーティング系リプライ](#クラスタルーティング系リプライ)参照。 |
| `-MISDIRECTED writer is <host:port>` | 書き込みが、このキーのスコープを所有しないノードに着地した。または`REPL.WAIT`がこのレプリカでread-your-writesを提供できなかった（タイムアウトか世代不一致）。 | [クラスタルーティング系リプライ](#クラスタルーティング系リプライ)参照。`REPL.WAIT`の場合は`<host:port>`のプライマリを読む。 |
| `-QUIESCED migrating to <host:port>` | スロットまたはスコープが移行中でこのノード上で凍結されている。またはノードが`<host:port>`への`FAILOVER`引き継ぎの最中。 | [クラスタルーティング系リプライ](#クラスタルーティング系リプライ)参照。 |
| `-OOM command not allowed when used memory > 'maxmemory'` | ポリシー`noeviction`で上限超過後に書き込み系コマンドが来た。 | `maxmemory`を上げる、退去ポリシーを設定する（例：`allkeys-lru`）、または`DEL`で空きを作る。既存データは無傷。 |
| `-READONLY You can't write against a read only replica.` | レプリカノードへ書き込みコマンドが送られた。 | プライマリへ送るか、ルーティングクライアントを使う。 |
| `-NOREPLICAS Not enough good replicas to write.` | `min_replicas_to_write = N`のプライマリから見える健全なレプリカがN未満。 | バックオフしてリトライ。レプリカを復旧する（[docs/availability.md](https://github.com/goliajp/kevy/blob/develop/docs/availability.md)参照）。 |
| `-NOREPLICAS primary lost quorum; writes fenced` | electクォーラムのプライマリがピアの厳密過半数へ到達できない（パーティションの少数派側）。書き込みはリース期間だけ自己フェンスする。 | バックオフしてリトライ——パーティションが治ればフェンスは解け、あるいは多数派が新プライマリを選出してルーティングクライアントが見つける。 |
| `-STALE replica is stale; read the primary or raise replica_max_staleness_ms` | 最後のプライマリハートビートが`replica_max_staleness_ms`の上限より古いレプリカへ読み取りが送られた。 | レプリカが追いつくまでプライマリを読むか、上限を上げる／無効化する。 |
| `-READONLY can't write against a read-only script` | `EVAL_RO`／`EVALSHA_RO`で評価されたスクリプトが書き込みを試みた。 | 書き込み可能な`EVAL`／`EVALSHA`側を使う。 |
| `-NOSCRIPT No matching script. Please use EVAL.` | `EVALSHA <sha>`がキャッシュにないスクリプトを要求した。 | 直接`EVAL`を呼ぶ（kevyは自動キャッシュ）か、先に`SCRIPT LOAD`する。 |
| `-LOADING kevy is loading the dataset in memory` | プライマリからフル再同期スナップショットを受信中のレプリカへ読み取りが送られた。 | 待ってリトライ（ウィンドウはスナップショット転送で有界）。ローディング中も`PING`、`INFO`、`HELLO`は応答されるので、ヘルスチェックと監視は動き続ける。 |
| `-ERR No such client address in the list` | レガシー形の`CLIENT KILL <addr:port>`がどのコネクションにも一致しなかった。 | `CLIENT LIST`で稼働中コネクションを列挙し、実在の`addr`で再発行する。フィルタ形（`CLIENT KILL ID\|ADDR\|LADDR …`）はエラーではなくカウント0を返す。 |

### kevyが決して送出しないプレフィックス

設計上、kevyはプロトコル層での認証・認可を行いません。次のRedis互換プレフィックスは意図的に不在です。

- `-NOAUTH`——`AUTH`コマンド面がない。
- `-WRONGPASS`——パスワード検査がない。
- `-NOPERM`——ACLシステムがない。
- `-MISCONF`——永続化の失敗（AOF追記やバックグラウンド保存）はstderrにログされ、ノードは一貫したインメモリ状態から配信を続けます。kevyは耐久性エラーをクライアント可視の応答には変えません。再起動時はディスクに届いた分をリプレイします。
- `-BUSY`——kevyに対話的な`SCRIPT KILL`はありません。スクリプトは代わりに命令バジェット（`[lua] time_limit_ms`、`0`＝無制限）で制限され、バジェット超過のスクリプトはその場で中断され、その`EVAL`は通常の`-ERR` Luaエラーを返します。暴走スクリプトが他のクライアントの介入を要するほどシャードを固めることはありません。

クライアントがこれらの発生を想定しているなら、kevy上では到達不能として扱ってください。アクセス制御はデプロイの境界に委譲されています（kevyはデフォルトで`127.0.0.1`にbindします）。

## WRONGTYPEの規則

`WRONGTYPE`リプライは、そのキーがコマンドの期待と異なるRedisデータ型ですでに存在することを意味します。規則は次の通りです。

- 型はキー作成時に決まり、キーが削除される（か期限切れになる）まで持続します。
- `DEL <key>`のあとに元のコマンドを実行すれば成功します（型をリセットしたことになります）。
- `EXPIRE`／`PERSIST`／`TYPE`／`OBJECT ENCODING`／`EXISTS`／`DEL`／`UNLINK`は型非依存で、`WRONGTYPE`を決して発生させません。
- 存在しないキーに対して`WRONGTYPE`が返ることはありません。キー不在の意味論は各コマンドの文書化された振る舞いに従います（`GET`はnilを返す、`LPUSH`はリストを作る、など）。

回復は常にどちらかです。別のキーを選ぶか、既存キーを`DEL`して意図した型で作り直すかです。

## クラスタルーティング系リプライ

これらのプレフィックスは、kevyがルーティングされたモード（クラスタまたはスコープ）で動いているときにだけ現れます。非クラスタのクライアントは一度も見ないかもしれません。

- **`-MOVED <slot> <host:port>`**——キーのハッシュスロットが恒久的に別ノードの所有であるときに発火します。クライアントは`<host:port>`へ再接続して再発行すべきです。クラスタ対応クライアント（`redis-cli -c`、クラスタモードの`ioredis`など）はリダイレクトを透過的に追い、スロットマップを更新します。
- **`-CROSSSLOT Keys in request don't hash to the same slot`**——複数キーコマンド（`MGET`、`MSET`、複数キーの`DEL`、複数`KEYS`の`EVAL`、`SUNIONSTORE`など）で、キー全部が同じスロットにハッシュされないときに発火します。共有の`{hashtag}`セグメントでキーを同居させるか、クライアント側でスロットごとのバッチに分割してください。
- **`-MISDIRECTED writer is <host:port>`**——キーのスコープを所有しないノードに書き込みが着地したときに発火します。ルーティングクライアントはリダイレクトを追います。手動のクライアントは`<host:port>`へ再接続してください。
- **`-QUIESCED migrating to <host:port>`**——スロットまたはスコープが移行中で、このノード上で凍結されている間に発火します。クライアントは`MOVED`と同様に扱い、`<host:port>`に対してリトライすべきです。移行が完了すれば、そのノードが権威をもって応答します。

ルーティングモデルの全体は[docs/](https://github.com/goliajp/kevy/tree/develop/docs)のプロトコルノートを参照してください。

## FAQ

**クライアントが`-MOVED`を致命的エラーとして扱うのですが——どう直せば？**
そのクライアントはクラスタ対応ではありません。クラスタ対応クライアント（`redis-cli -c`、`Cluster`付き`ioredis`、`RedisCluster`付き`redis-py`、kevyのルーティングクライアント）へ乗り換えるか、ドライバをラップして`MOVED`を捕まえ、メッセージ内のホストへ再接続して再発行してください。

**単一キーのコマンドに`-CROSSSLOT`が返ってきました——バグですか？**
いいえ。`CROSSSLOT`は複数キーコマンドでしか発火しません。単一キーに見える呼び出しで出たなら、そのコマンドは実際には複数キーです（2つの`KEYS`を持つ`EVAL`、ソース＋宛先の`SUNIONSTORE`など）。`{tag}`記法でスロットの同居を強制するか、呼び出しを分割してください。

**`-OOM`が出ました——データは壊れていますか？**
いいえ。`-OOM`はコマンド受理の時点で拒否されており、その書き込みは一切着地していません。キー空間はコマンドの前と正確に同じ状態です。空きを作って（`DEL`／退去ポリシー設定／`maxmemory`引き上げ）リトライしてください。

**`-LOADING`が繰り返し返ってきます——どれくらい待つべき？**
フル再同期のスナップショット転送にかかる時間だけです（データセットのサイズとリンク速度に比例）。ローディング中も`PING`、`INFO`、`HELLO`は応答され（`INFO replication`は`loading:1`を報告します）、ヘルスチェックは動き続けます。レプリカがこの状態に入るのは、プライマリのバックログから遠く離れて再接続したときだけです。`-LOADING`が恒常的に再発するなら、`[replication] replication_buffer_size`を上げるか、リンクを調査してください。

**キューしたMULTIコマンドが`-EXECABORT`を返しました——どれかの書き込みは適用された？**
いいえ。`EXECABORT`はトランザクションがバッチとして拒否されたことを意味し、キューされた列の何も実行されていません。問題のコマンドを直して`MULTI`を開き直してください。

## 拡張面（IDX. / VIEW. / FEED.）

拡張verbも同じプレフィックス契約に従います。すべてのエラーは自己説明的です。verbと対象オブジェクトを名指しし、発見面を指し示します（これに当たったエージェントはインバンドで回復できます）。

| プレフィックス | 送出タイミング | 回復 |
|---|---|---|
| `ERR <VERB> '<name>': bad arguments — run COMMAND DOCS <VERB> for the syntax` | IDX./VIEW. verbの引数パースが失敗した | `COMMAND DOCS <verb>`が完全な構文文字列を返す |
| `ERR no such index '<name>' (IDX.LIST enumerates them)` | クエリが存在しないインデックスを名指しした | `IDX.LIST`／`VIEW.LIST`がカタログを列挙する |
| `INDEXBUILDING index '<name>' is still building (poll IDX.LIST until state=ready)` | クエリが作成後バックフィルとレースした | `IDX.LIST`の`state`をポーリング。docs/migration.md参照 |
| `INDEXOVERBUDGET index '<name>' build exceeded MAXMEM (raise maxmemory or DROP the index)` | 構築がメモリ予算に当たった | `maxmemory`を上げるか`IDX.DROP` |
| `FEEDRESYNC <gen> <tail>` | FEEDカーソルがもうサービスできない（世代の増加か、バックログ超過） | 新しいスナップショット＋返されたカーソルから消費を再開。docs/cdc.md参照 |

## このカタログの更新手順

`-<PREFIX> ...`リプライを送出するコードパスを追加・変更したら：

1. [コアリファレンス](#コアリファレンス)の行を更新します（または新しい行を追加）。
2. 新しいプレフィックスを導入したなら、ワイヤレベルのカオステスト[crates/kevy/tests/wire_torture_chaos.rs](https://github.com/goliajp/kevy/blob/develop/crates/kevy/tests/wire_torture_chaos.rs)を拡張します。
3. それを出荷するリリースの[CHANGELOG.md](https://github.com/goliajp/kevy/blob/develop/CHANGELOG.md)に変更を記載します。

エラーはクライアント契約の一部です。メッセージの黙った変更は、それをパターンマッチするエコシステムのライブラリを壊し得ます。

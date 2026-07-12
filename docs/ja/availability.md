# 可用性

kevyのデプロイが、ノード障害を越えて書き込み可能・読み出し可能であり続けるための仕組みです。トポロジ、「最速・非同期」から「クォーラムでフェンスされた」までの整合性ラダー、計画的フェイルオーバーとクラッシュフェイルオーバー、そして各段階でクライアントが見る正確なエラー契約を扱います。

本書は[`docs/replication.md`](replication.md)（ストリーミングの機構）の上に立っています。プライマリ/レプリカのペアを一度も立ち上げたことがないなら、先にそちらを読んでください。kevyノードを1台しか動かしていないなら、あなたに関係するのはラダーの最初の段だけです。

## トポロジ

### シングルノード

レプリケーションの設定なし（`role = "standalone"`、これがデフォルト）。すべての書き込みはローカルストアに着地した時点でackされます。耐久性の担当は[`docs/persistence.md`](persistence.md)（AOF + RDB）です。フェイルオーバーは存在しません。可用性はプロセスの寿命と等しくなります。

### プライマリ + レプリカ

```toml,fragment
# primary                          # each replica
[replication]                      [replication]
role = "primary"                   role = "replica"
                                   upstream = "primary.internal:16004"
```

プライマリは適用されたミューテーションをシャードごとにすべてストリーミングし、レプリカはそれを適用して読み出しを返します（クライアントからの書き込みには`-READONLY`が返ります）。配線の形は2つあります。

- **Fleet**（デフォルト）：upstreamは完全なkevyサーバーです。ローカルの各シャードが`(host, port_base + shard_index)`に接続し、シャードごとに1本のストリームを張ります。
- **`single_source = true`**：upstreamは1つのポート上の1本のストリームです（`with_embed_writer`による組み込みライター）。1本のルーティングランナーが、キーのハッシュに従ってフレームをローカルシャードへ振り分けます。[`docs/replication.md`](replication.md)の*組み込みをプライマリにする*節を参照してください。

このトポロジでのフェイルオーバーは**手動**です。生き残った側で`REPLICAOF NO ONE`を実行し、残りを再ターゲットします。

### 3ノードのelectクォーラム

```toml
# every node adds the same [cluster] block
[cluster]
node_id         = "n1"                  # unique per node
elect_port_base = 6204
peers           = "n1@10.0.0.1:6204:6004,n2@10.0.0.2:6204:6004,n3@10.0.0.3:6204:6004"
```

メンバーシップは**静的**（オペレータが宣言した`peers`テーブル）、ロールは**動的**です（選挙がそのテーブルの内側でプライマリを動かします）。クォーラムは`N/2 + 1`です。N=2はいかなる障害にも耐えられません（どちらのノードが落ちても、生き残った側は読み取り専用にロックされます）。フェイルオーバーが必要なデプロイはN ≥ 3にしてください。

peerの記法は拡張形の`id@host:elect_port:client_port`を使ってください。選挙のトラフィックはelectポートに乗り、再ターゲットと`-MISDIRECTED`応答はクライアントポートを使います。レガシーな2フィールド形式ではクライアントポートがelectポートと等しいとみなされますが、それが望みどおりであることはまずありません。

**書き込み権限は選挙だけが与える。** electクォーラムの中では、`[replication] role = "primary"`は初期の*希望*にすぎません。primaryとして構成されたクォーラムメンバーは、どれも**読み取り専用**で起動し、選挙に勝つまで書き込みを保留します——コールドスタートも例外ではなく、最初の書き込みが受理されるまでクラスタは選挙1ラウンド分（数秒）を支払います。この無条件のクランプこそが、古典的な事故——「空のプライマリが再起動してクラスタを消し去る」——を防ぎます。クラッシュしてディスクを失ったノードが、configだけを根拠に書き込み可能な状態で戻ってくることは決してありません。

## 整合性ラダー

レプリケーションはデフォルトで非同期です。以下の各段は、1つのverbまたは1つのconfigキーと引き換えに、より強い保証を買います。ある読み出し・ある書き込みに必要なものだけ、正確に支払ってください。

| 段 | 仕組み | 保証 | コスト |
|---|---|---|---|
| 0 | （デフォルト） | プライマリはローカル適用後にackする。レプリカは遅れる | なし |
| 1 | `REPL.TOKEN` + `REPL.WAIT` | 選んだレプリカ上でのread-your-writes | レプリカ上でブロックする呼び出し1回 |
| 2 | `WAIT n timeout` | 書き込みがプライマリ上にあり、**かつ**n台以上のレプリカにackされている | プライマリ上でブロックする呼び出し1回 |
| 3 | `replica_max_staleness_ms` | レプリカは境界より古い読み出しを決して返さない（`-STALE`） | ラグのスパイク中、読み出しはプライマリへフェイルオーバーする |
| 4 | `min_replicas_to_write` | フレッシュなレプリカがn台なければプライマリは書き込みを拒否する（`-NOREPLICAS`） | 書き込み可用性がレプリカの健全性と結合する |
| 5 | クォーラムリース（electで自動） | 分断されたプライマリは自分の書き込みをフェンスする（`-NOREPLICAS`） | 分断中、リースウィンドウのあいだ書き込みが止まる |

### read-your-writes：`REPL.TOKEN` / `REPL.WAIT`

```
REPL.TOKEN
REPL.WAIT gen offset [gen offset ...] [TIMEOUT milliseconds]
```

プライマリに書き、そこで`REPL.TOKEN`を呼びます。シャードごとに`(generation, offset)`の組が1つずつ返ります——これは生きたフィードの末尾であり、構成上あなたの書き込みを必ず含みます。そのトークンを丸ごと、これから読もうとしているレプリカ上の`REPL.WAIT`に渡してください。全シャードがそこまで**適用**するまでブロックし、`+OK`を返します。以降、そのコネクション上の次の読み出しはあなたの書き込みを観測します。`TIMEOUT`のデフォルトは1000 msで、`0`およびそれより大きい値はすべて60 sで打ち切られます。

`generation`の側は誤用防止の要です。これは1つの途切れないオフセット履歴を識別します（CDCフィードが使うのと同じgenerationです——[`docs/cdc.md`](cdc.md)を参照）。フェイルオーバー、`FLUSHALL`、クラッシュ再起動はこれを進めるため、旧プライマリのオフセット空間に対して発行されたトークンが、新しい空間で偽って充足されることはあり得ません。generationが食い違えば`REPL.WAIT`は即座に`-MISDIRECTED writer is <primary>`を返し、クライアントはプライマリを読みにフォールバックします。タイムアウトも同じ応答を返します。どちらにせよ「ライターを読みに行け」が唯一の復帰路です。

プライマリ上では`REPL.WAIT`は即座に`+OK`を返す（すでにライターと話しているため）ので、ルーティングクライアントを通して無条件に発行しても安全です。

### `WAIT` — 耐久性ではなく、レプリカのack

```
WAIT numreplicas timeout
```

**すべてのシャードの**`master_repl_offset`（バリアが張られた時点で凍結される）が、少なくとも`numreplicas`台のレプリカにackされるまでブロックします。タイムアウトすれば打ち切られ、シャードをまたいだackレプリカ数の最小値を返します。全シャードバリアであることは意図的です。kevyの書き込みはどのシャードにも着地しうるので、シャードごとに数えることだけが決して間違わない答えなのです。`timeout 0`はRedisの「永遠に待つ」ですが、60 sでハードキャップされます。レプリカ上で発行すると`WAIT`は`-ERR WAIT cannot be used with replica instances`を返します。

**`WAIT`は耐久性ではありません。** レプリカのackが意味するのは、フレームがそのレプリカの適用パイプラインに届いたことだけであり、どこかでfsyncが起きたことではありません。`WAIT 1`が買えるのはこれです。書き込みが2ノード上に存在するので、*その後の選挙が最も進んだ候補を選ぶ限り*、単一ノードの喪失を生き延びます——そして実際にそう選びます（後述のクラッシュフェイルオーバー）。電源断に対する耐久性が要るなら、レプリケーションとプライマリ上のAOFを組み合わせてください。

### 有界ステイルネス：`-STALE`

```toml
[replication]
replica_max_staleness_ms = 2500     # 0 = off (default)
```

最後にプライマリのハートビートを受けてから境界より長く経ったレプリカは、`-STALE replica is stale; read the primary or raise replica_max_staleness_ms`で読み出しを拒否します。ハートビートは1 Hzでレプリケーションストリームに乗るため、約2 s未満の境界は健全なリンクでも発火します。ゲートは2500 msを使っています。レプリカはハートビートが再開した瞬間に回復します。オペレータの操作は不要です。

### 書き込みゲート：`min_replicas_to_write`とクォーラムリース

```toml
[replication]
min_replicas_to_write = 1           # 0 = off (default)
```

Redisを手本にしたヒューリスティックです。健全なレプリカがN台未満のとき、プライマリは`-NOREPLICAS Not enough good replicas to write.`で書き込みを拒否します。健全とは、生きたレプリケーションコネクションがあってackを返しており、**かつ**その最新のackが`min_replicas_max_lag_ms`（デフォルト10 000 ms）より新しいことを指します。ackを止めた停滞レプリカは、TCPコネクションが上がったままでも、この数から自然に落ちていきます。これは「プライマリが虚空に書き込む」窓を閉じますが、スプリットブレインの保証では**ありません**。分断の両側が、それぞれ自分のレプリカを見ることはあり得ます。

本物のフェンスは**クォーラムリース**で、electクォーラムでは自動です。選挙ハートビートがリースウィンドウ（= `down_after`、5 s）以内にピアの厳密な過半数へ届かなくなったプライマリは、`-NOREPLICAS primary lost quorum; writes fenced`で自分の書き込みをフェンスし、分断が癒えたらフェンスを解きます。`WAIT`やトークンと組み合わせれば、「少数派側が黙って書き込みを吸い込んだ」は最大でも1リースウィンドウ分に圧縮され、しかもその窓の中の書き込みはすべて、黙って分岐するのではなく*大きな音を立てて*失敗します。

## フェイルオーバー

### 計画的：`FAILOVER` verb

```
FAILOVER host port [TIMEOUT ms]      # host:port = the target replica's CLIENT address
FAILOVER ABORT
```

プライマリ上で実行します。即座に`+OK`を返し、引き継ぎはバックグラウンドスレッドで走ります（Redisの`FAILOVER`と同じく非同期です）。

1. **Quiesce** — 新しいクライアント書き込みはすべて`-QUIESCED migrating to <host:port>`を返します。[`kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw)クライアントはこれをバックオフ付きで再試行するので、ライターは失敗せず、単に止まります。
2. **Drain** — 旧プライマリはターゲットの`INFO replication`をポーリングし、`master_link_status:up`かつ`slave_lag_frames:0`になるのを待ちます。書き込みがquiesceされているので、収束したゲージは正確です——ここがゼロロスの要となる手順です。
3. **Promote + follow** — ターゲットに`REPLICAOF NO ONE`が送られ（そのフィードgenerationが進み、古いトークンがフェンスされます）、続いて旧プライマリが自分をターゲットのレプリケーションポートへ再ターゲットして、読み取り専用のレプリカになります。
4. Quiesceを解きます。まだ旧ノードを向いている迷子の書き込みには`-READONLY`が返り、再ルーティングされます。

`FAILOVER ABORT`は昇格前ならいつでもquiesceを解除します。バックグラウンドスレッドはそれに気づいて手を引きます。ターゲットが`TIMEOUT`（デフォルト10 000 ms）以内にdrainしきらなければ、quiesceはロールバックされ、ノードはプライマリの職務を再開します。失敗した試行のコストは、書き込み可用性のまばたき1回分だけで、それ以上は何もありません。

アドレッシング上の制約が1つあります。引き継ぎは`クライアントポート + 10000`へ再ターゲットするため、ターゲットはデフォルトの`listen_port_base`で動いている必要があります（後述のポート規約）。

### クラッシュ：クォーラム選挙

全ノードに`[cluster]`ブロックが構成されていれば、死んだプライマリはオペレータなしで検出され、置き換えられます。

1. ピアたちが`down_after`（5 s）のあいだ選挙ハートビートを受け取れず、そのノードにDOWNのフラグを立てます。
2. 資格のあるレプリカが立候補します。生存ピアの中で**最大のレプリケーションオフセット**（シャードをまたいだ適用済みストリーム位置の合計）を持っていなければならず、同点は最小の`node_id`が破ります。オフセット最良の順序づけこそが、`WAIT`でackされた書き込みを生存者に変える仕組みです——あなたのackされた書き込みを持つレプリカが、持たないレプリカより上位に立ちます。
3. 候補は`election_timeout`（3 s）以内にクォーラム分の`ACCEPT`を集める必要があります。エポックと投票は、ノードを出る*前に*`<data_dir>/elect.meta`へ永続化されるため、クラッシュ再起動しても二重投票はあり得ません。
4. 勝者は`ANNOUNCE`をブロードキャストし、自分のランナー群を止めて（書き込みが開きます）、フィードgenerationを進めます。敗者は自動的にレプリケーションのupstreamを勝者へ再ターゲットします。

**MTTR ≈ `down_after` + 選挙1ラウンド** ——出荷時のタイミングでおよそ5〜8 sです。ゲートは書き込み再開まで含めて端から端まで30 sで上限を切っています。選挙のタイミング（`hb_interval` 200 ms、`down_after` 5 s、`election_timeout` 3 s）は本リリースでは固定定数であり、configキーではありません。

**旧プライマリが戻ってきたとき。** 再起動ロールのクランプにより、読み取り専用で起動します（前述の「書き込み権限は選挙だけが与える」）。選挙が現在のプライマリを教え、旧プライマリは再ターゲットしてハンドシェイクします。分断後・死亡前に吸い込んでしまった書き込みがあれば、それは*分岐した末尾*を形成します。そのストリーム位置は新プライマリより先に進んでおり、新プライマリは唯一安全な答えを返します——**フォークを破棄し**、フルスナップショットを送り、ライブフレームを再開する。再合流したノードは多数派の履歴に収束し、分岐した書き込みは消えます（定義上それらは`WAIT`でackされていません。フォークとはまさに、レプリケートされなかった末尾のことです）。

故障ノードが**自動で置き換えられることはなく**、メンバーシップが実行中に変わることもありません。ハードウェアの交換とは、全ノードの`peers`を更新して再起動することです。動的メンバーシップ、マルチプライマリ、クロスDCは憲章の外です。

## ライターとリーダーのためのエラー契約

完全なカタログは[`docs/error-replies.md`](error-replies.md)にあります。この表はその可用性スライス——トポロジのライフサイクルの各点でクライアントが何を見て、何をすべきかです。`kevy-cluster-rw::ReadWriteClient`はこれらの振る舞いをすべて実装済みです（書き込みはプライマリへ、読み出しはレプリカへラウンドロビン、`MISDIRECTED`/`QUIESCED`ではリダイレクトに追従、プライマリを強制する整合読み出しパスも持ちます）。

| 応答 | 話している相手 | いつ | クライアントの行動 |
|---|---|---|---|
| `-READONLY You can't write against a read only replica.` | レプリカ（降格した、あるいはクランプされた元プライマリを含む） | `replica_read_only = true`（デフォルト）のもとでのクライアント書き込み全般 | 書き込みはプライマリへ送る。ルーティングクライアントはプライマリを再解決する |
| `-QUIESCED migrating to <host:port>` | `FAILOVER`の途中にあるプライマリ | quiesce窓（引き継ぎ手順1〜3） | バックオフして再試行する。引き継ぎが着地したら`<host:port>`に追従する |
| `-MISDIRECTED writer is <host:port>` | レプリカ（`REPL.WAIT`）、または非所有ノード（スコープ付き書き込み） | read-your-writesが提供できない（タイムアウト / generation不一致）、あるいはスコープでルーティングされる書き込み | `<host:port>`——現在のライター——で読む（または書く） |
| `-NOREPLICAS Not enough good replicas to write.` | `min_replicas_to_write`を設定したプライマリ | 健全なレプリカがN台未満 | バックオフして再試行する。レプリカの担当者を呼び出す |
| `-NOREPLICAS primary lost quorum; writes fenced` | 分断の少数派側にいるクォーラムプライマリ | クォーラムリースを失った | バックオフして再試行する。分断が癒えるか、多数派が新しいプライマリを選ぶかのどちらかで、ルーティングクライアントがそれを見つける |
| `-STALE replica is stale; read the primary or raise replica_max_staleness_ms` | ステイルネス境界を持つレプリカ | プライマリのハートビートが境界より古い | レプリカが追いつくまでプライマリを読む |
| `-LOADING kevy is loading the dataset in memory` | フル再同期の途中にあるレプリカ | スナップショットの送出がレプリカのキー空間を丸ごと置き換えている | 待って再試行する。窓は送出時間で有界。`PING` / `INFO` / `HELLO`は依然として答えるので、ヘルスチェックは通り続ける |

経験則が2つあります。これらはどれも**設計上リトライ可能**であり（何も適用されていません）、どれもルーティングクライアントが自己修復するのに必要なトポロジの真実を名指しします。人間の介在を要するものは1つもありません。

## 運用

### ポート規約

| プレーン | ポート | 備考 |
|---|---|---|
| クライアントRESP | `server.port`（例：6004） | クライアントと`peers`のclient-portが指すアドレス |
| レプリケーション | `listen_port_base + shard_i`。デフォルトのbase = クライアントポート + 10000 | `nshards`個の連続ポート。v3.15以降、**レプリカもこのリスナーをbindします**（昇格の対称性） |
| 選挙 | `elect_port_base`。デフォルト = クライアントポート + 200 | ノードごとに1本のコントロールプレーンリスナー |

自動再ターゲット（選挙）も`FAILOVER`も、`クライアントポート + 10000`というレプリケーションの規約を前提にしています。フェイルオーバーを有効にするデプロイでは`listen_port_base`をデフォルトのままにしてください。1台のホストで複数インスタンスを動かす場合は、クライアントポートを最低`nshards`離してください。そうしないとインスタンス同士のレプリケーションポートレンジが衝突します。

### configキー

`[replication]`（[`crates/kevy-config/src/replication.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-config/src/replication.rs)を参照）：

| キー | デフォルト | 意味 |
|---|---|---|
| `role` | `"standalone"` | `"primary"`はレプリカへストリーミングする。`"replica"`は`upstream`から引く。standalone = サブシステム休眠 |
| `upstream` | 未設定 | レプリカ専用。プライマリのレプリケーションポートベースの`host:port` |
| `listen_port_base` | `0`（= クライアントポート + 10000） | シャード`i`はbase + `i`でレプリケーションをbindする |
| `replication_buffer_size` | `256mb` | シャードごとのリングバックログ。この中での再接続はスナップショットを飛ばせる |
| `reconnect_window_ms` | `60000` | 切断されたレプリカのスロットを保持する時間 |
| `replica_read_only` | `true` | レプリカ上のクライアント書き込みを拒否する（`-READONLY`）。`CONFIG SET`が脱出口 |
| `replica_max_staleness_ms` | `0`（オフ） | ラダー3段目。境界を超えた読み出しは`-STALE` |
| `min_replicas_to_write` | `0`（オフ） | ラダー4段目。健全なレプリカがN台未満なら`-NOREPLICAS` |
| `min_replicas_max_lag_ms` | `10000` | 4段目の鮮度ウィンドウ。最新のackがこの境界より新しいレプリカだけが健全とみなされる。停滞レプリカはもはや`min_replicas_to_write`を満たさない |
| `single_source` | `false` | シャードごとのfleetではなく、1本のストリームのupstream（組み込みライター） |

`[cluster]`の選挙キー（[`crates/kevy-config/src/cluster.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-config/src/cluster.rs)を参照）：`node_id`（32 BまでのASCII、一意）、`elect_port_base`、`peers`（`id@host:elect_port[:client_port],...`）。`node_id`と`peers`の両方が設定されない限り、electorは休眠したままです。

### 可観測性

**レプリカ**上の`INFO replication`：

| フィールド | 真実の出どころ |
|---|---|
| `role:slave` + `master_host` / `master_port` | 生きているupstream（実行時の`REPLICAOF`がconfigに優先する） |
| `master_link_status` | 3 s以内にストリームのハートビートが着地していれば`up`、なければ`down` |
| `master_last_io_seconds_ago` | 最後のハートビートからの経過時間 |
| `slave_read_only` | `-READONLY`のゲート |
| `slave_repl_offset` | 適用済みのストリーム位置 |
| `slave_lag_frames` | プライマリが広告する末尾 − 適用済み。**0なら追いついている** |

**プライマリ**上の`INFO replication`：

| フィールド | 真実の出どころ |
|---|---|
| `role:master`、`master_repl_offset` | **全シャードで合算した**ストリーム末尾（選挙のオフセット最良の順序づけが使うのと同じ規約） |
| `connected_slaves` | 生きたコネクションを持つ、別個のレプリカプロセスの数（1つのレプリカのシャードごとのストリームは1エントリに畳まれる） |
| `slaveN:ip=…,port=…,state=…,offset=…,sent=…,lag=…` | レプリカプロセスごと、ピアアドレスで識別される。`state`は**すべての**シャードストリームがackした時点で`online`（1つでも未ackなら`syncing`）、`offset`はシャードをまたいで合算したackされた位置、`lag`はフレーム数 |

プロセスをまたいで比較できるのは、レプリカ自身の`slave_lag_frames`ゲージとデータそのものです。異なる2プロセスのINFO出力どうしでオフセットの算術をすることに意味があるのは、1つのgenerationの内側だけです。

`ROLE`は同じ真実をRedisの配列の形で返します（`master` + オフセット + レプリカごとの`[ip, port, acked-offset]`、あるいは`slave` + upstream）。electクォーラムが構成されていれば、生きた選挙ロールが`REPLICAOF`の状態にもconfigにも優先します。実務でのラグの見張り方は、レプリカ上で`slave_lag_frames`をポーリングし（ステイルネス予算を超えて非ゼロが続いたらアラート）、プライマリ上で`WAIT 1 <小さいタイムアウト>`を「少なくとも1台のレプリカが追随しているか」の安価なend-to-endプローブとして使うことです。

### ゲート

本書が約束することはすべて実行可能です。[`bench/availgate.sh`](https://github.com/goliajp/kevy/blob/develop/bench/availgate.sh)は実プロセスに対して13個のclampを走らせます。フェーズ1（適用中のREADONLY、オフセット/ラグの真実、リンクdown/up検出、レプリカごとのackの真実、min-replicas）、フェーズ2（3ノードのクラッシュフェイルオーバーとMTTR上限、再起動ロールのクランプとフォーク破棄による再合流）、フェーズ3（`WAIT`の真実、20/20ラウンドのread-your-writesとfuture-tokenのMISDIRECT、SIGSTOPされたプライマリに対する`-STALE`、クォーラムリースのフェンスと再開）、フェーズ4（フル再同期の送出窓をまたぐ`-LOADING`契約、`PING`の免除）です。ここに書かれた主張とゲートが食い違ったら、**ゲートが勝ちます**。ドキュメントのバグとして報告してください。

## 関連項目

- [`docs/replication.md`](replication.md) — ストリームの機構、スナップショット送出、組み込みをレプリカにする、バックログのサイジング。
- [`docs/cdc.md`](cdc.md) — `REPL.TOKEN`が共有する`(generation, offset)`カーソルのモデル。
- [`docs/error-replies.md`](error-replies.md) — エラーの完全なカタログ。
- [`docs/persistence.md`](persistence.md) — 物語の耐久性の側（`WAIT`はそれではありません）。
- [`crates/kevy-elect/docs/protocol.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect/docs/protocol.md) — 選挙のワイヤプロトコル。
- [`crates/kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) — このエラー契約を丸ごと実装したクライアント。

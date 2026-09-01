# Japanese content for kevy.golia.jp.
#
# Written as Japanese, not translated from en.py. The English is the source for
# the facts; the sentences are rebuilt so they land the way Japanese technical
# prose lands. Polite form (desu/masu), matching the guides under docs/ja.
#
# The site answers a visitor's questions in the order they ask them. It is not a
# place to narrate our engineering: how the harness turned out to be quantised,
# which optimisation turned out to be a tax rather than a bottleneck — that
# lives in bench/ and in the commit log. A visitor here has a problem and twenty
# seconds.
#
# What DOES stay, because it changes what a reader decides:
#   * where kevy wins by a lot, and where by almost nothing (LPUSH: 12%);
#   * what it refuses to do (no cluster, no AUTH, no TLS);
#   * which commands do not behave the way Redis's documentation says.
#
# Every "text" and "code" field is byte-identical to en.py. The commands in them
# are executed against a real server by tools/check_site_commands.py, so a single
# stray character breaks that gate. Translate the prose, never the code.
#
# Punctuation is gated. Japanese prose takes 「、」「。」「(」「)」「——」 and never
# an ASCII comma or full stop next to a Japanese character. ASCII is correct
# inside a `code` element, inside code samples, and in numbers. Run:
#   python3 tools/check_cjk_punct.py tools/site_content/ja.py

PAGES = {}

# ── / ───────────────────────────────────────────────────────────────────────

PAGES[""] = {
    "title": "kevy — AI システムのためのデータレイヤ",
    "desc": "AI システムのための Redis 互換データレイヤ。プロトコルは同じまま、スループットは上。そしてベクトル検索、全文検索、インデックス、ビュー、変更フィードがひとつのエンジンに。実際に触ってみてください——このページのターミナルは本物のエンジンで、あなたのタブの中で動いています。",
    "foot": "GOLIA",
    "blocks": [
        {
            "t": "hero",
            "h1": "AI システムのための<br>データレイヤ。",
            "lede": (
                "Redis 互換——クライアントは、そのままつながります。どの操作でも、"
                "より高速です。そしてベクトル検索、全文検索、インデックス、ビュー、"
                "変更フィードは、周りに並べた 4 つのサービスではなく、<b>エンジンの"
                "中に</b>あります。<b>このターミナルは本物です</b>。同じエンジンを "
                "WebAssembly にコンパイルしたものが、このタブの中で動いています。"
            ),
            "ctas": [
                {"label": "cargo install kevy", "href": "#start"},
                {"label": "何ができるのか", "href": "#code"},
                {"label": "playground を開く", "href": "#try"},
            ],
            "live_term": {
                "hint": "コマンドを入力——SET、GET、TTL、INCR、KEYS、SUBSCRIBE、PUBLISH…",
                "chips": ['SET session:7f3a \'{"user":"ada"}\' EX 30', 'GET session:7f3a', 'TTL session:7f3a', 'INCR hits', 'KEYS *', 'SUBSCRIBE news', 'PUBLISH news deployed'],
            },
        },
        {
            "t": "tabs",
            "id": "code",
            "tone": "deep",
            "eyebrow": "他に何ができるのか",
            "h2": "ひとつのエンジン。AI システムに必要なスタックのすべて。",
            "intro": "以下のコマンドはすべて、このページを出す前に、CI が実際のサーバーに対して実行しています。タブを切り替えてみてください——kevy を使うとは、こういうことです。",
            "items": [
                {
                    "label": 'ベクトル',
                    "code": '# an HNSW index over your keys — declared once,\n# kept current by the write path\nIDX.CREATE idx:sem ON PREFIX doc: FIELD vec TYPE vector KIND ann DIM 768 DISTANCE cosine M 16 EF 200\n\nHSET doc:4410 title "Ada on pipelining" vec "<768 f32, little-endian>"\n\n# nearest ten. no separate vector database, no sync job.\nIDX.QUERY idx:sem KNN "<query vector>" LIMIT 10\n-> 1) "doc:4410"\n   2) "doc:9982"\n',
                    "note": '埋め込みはあなたが持ち込み、kevy がそれを保存し、索引を張り、検索します。エンジンにモデルはありません。それは意図した選択です。',
                    "go": 'エージェントの記憶と RAG',
                    "href": 'use/ai/',
                },
                {
                    "label": '全文検索',
                    "code": 'IDX.CREATE idx:ft ON PREFIX doc: FIELD title TYPE str KIND text\n\nIDX.QUERY idx:ft MATCH "pipelining"\n-> 1) 1) "doc:1"\n      2) "0.2877"          # BM25 score\n\n# hybrid: fuse the text ranking with the vector ranking\nIDX.QUERY HYBRID idx:ft MATCH "pipelining" idx:sem KNN "<vector>" LIMIT 20 RRFK 60',
                    "note": 'CJK のトークン化を備えた BM25。ベクトルが索引している、同じキーの上で。',
                    "go": '検索の仕組み',
                    "href": 'use/ai/',
                },
                {
                    "label": 'インデックス',
                    "code": 'HSET order:1001 customer 881 status open  total 4400\nHSET order:1002 customer 881 status paid  total 8400\n\nIDX.CREATE idx:cust   ON PREFIX order: FIELD customer TYPE i64 KIND range\nIDX.CREATE idx:status ON PREFIX order: FIELD status   TYPE str KIND range\n\n# the read that would have been a SQL query\nIDX.QUERY COMPOSE AND idx:cust EQ 881 idx:status EQ open\n-> 1) "0"\n   2) 1) 1) "order:1001"\n',
                    "note": '絞り込んだ読み取りは、参照のままです。クエリプランナも、スキャンもありません。',
                    "go": 'データベースなしで読みを捌く',
                    "href": 'use/app-store/',
                },
                {
                    "label": 'ビュー',
                    "code": '# the answer, kept current by the WRITE path\nVIEW.CREATE v:open881 QUERY ( AND idx:cust EQ 881 idx:status EQ open ) ORDER BY idx:cust\n\nVIEW.QUERY v:open881\n-> 1) "0"\n   2) 1) "order:1001"  2) "881"\n\n# reads never recompute it; writes keep it fresh',
                    "note": 'ほとんどのアプリケーションが ORM に本当に求めているもの。',
                    "go": 'マテリアライズドビュー',
                    "href": 'use/app-store/',
                },
                {
                    "label": 'テーブル',
                    "code": '# a table is a declaration — compiled to named indexes, once\nTABLE.DECLARE user PREFIX u: PK id COLUMN id str COLUMN name str COLUMN age i64 COLUMN dept str INDEX age range VALUES dept name ORDERPATH by_dept_age ON dept THEN age DESC\n\nHSET u:1 id 1 name ada age 34 dept eng\n\n# the ORDER BY dept, age DESC walk — one composite index, no planner\nIDX.QUERY user.by_dept_age WHERE dept EQ eng LIMIT 20 FIELDS name age',
                    "note": '型付きカラム、セカンダリインデックス、複合 ORDER BY パス——kevy-cli sql compile なら PG/MySQL のスキーマファイルまでコンパイルできます。ランタイム SQL も join もありません。それは Postgres の仕事です。',
                    "go": '単一テーブルの配信',
                    "href": 'use/app-store/',
                },
                {
                    "label": 'RAM を超える',
                    "code": '# kevy.toml — a RAM budget for the whole store\n[tiering]\nbudget = "70%"               # or "4gb", or "auto"\n\n# past the budget, the coldest values spill to a disk log\n# and page back on access. a cold key is an ordinary key:\nGET archive:2019:q3          # pays one disk read, same reply\nTTL archive:2019:q3          # metadata answers from RAM\nSCAN 0 MATCH archive:*       # sees cold keys — one key table',
                    "note": 'RAM がキー数の上限を、ディスクがデータ量の上限を決めます。AOF の永続化コントラクトは無変更です。v1 で退避するのは文字列とハッシュ——リスト、セット、ストリームはホットのままです。',
                    "go": 'ティアリングの仕組み',
                    "href": 'docs/tiering/',
                },
                {
                    "label": '変更フィード',
                    "code": '# tail every write from another process — or an agent.\n# [feed] enabled = true in kevy.toml\nFEED.SHARDS                 -> (integer) 16\nFEED.TAIL 0                 -> 1) (integer) 1     # generation\n                               2) (integer) 1     # offset\nFEED.READ 0 1 0 COUNT 2     -> the writes themselves, replayable',
                    "note": '再開可能なオフセット。ポーリングするものも、取りこぼすものもありません。',
                    "go": '変更フィード',
                    "href": 'use/ai/',
                },
                {
                    "label": 'どこでも',
                    "code": '# a 16-core server\ncargo install kevy && kevy --port 6379\n\n# inside your binary — no socket, no process\nlet db = Db::open("data/")?;\ndb.set(b"k", b"v", None)?;\n\n# a browser tab — 481 KB, persists to OPFS\nconst db = await open({ persist: { name: "app" } });\n\n# a microcontroller — no OS, no allocator\nlet mut store = Store::new_in(&mut arena);',
                    "note": '4 つの場所すべてで、同じエンジン、同じコマンドです。',
                    "go": 'kevy を組み込む',
                    "href": 'use/embedded/',
                },
            ],
        },
        {
            "t": "bars",
            "id": "swap",
            "eyebrow": "なぜ Redis を置き換えられるのか",
            "h2": "プロトコルは同じ。スループットは上。",
            "intro": (
                "RESP2 と RESP3、188 個のコマンド——redis-cli も、クライアント"
                "ライブラリも、そのままつながります。1 台のマシン、16 コア、"
                "ループバック、5 回実行した中央値です。"
            ),
            "rows": [['GET', 7800299, 5597865, '1.39×', False], ['SET', 6918058, 2573396, '2.69×', False], ['INCR', 6133940, 3459395, '1.77×', False], ['SADD', 5600597, 3690483, '1.52×', False], ['HSET', 4287217, 3021325, '1.42×', False], ['LPUSH', 3213470, 2862374, '1.12×', True], ['ZADD', 3053101, 2773929, '1.10×', True]],
            "us": "kevy 6.2.2",
            "them": "Redis 8",
            "thin": "15% 未満——決めるのはエンジンではなく、あなたのワークロードです",
            "note": (
                "<b>LPUSH と ZADD は、12% と 10% しか上回っていません。</b>リストや"
                "ソート済みセットがホットパスなら、速さは乗り換える理由になりません。"
                "<a href=\"~/benchmarks/\">valkey や Dragonfly も含めた、完全な表は"
                "こちら。</a>移行はコマンド 3 つ——<a href=\"~/migrate/\">export、"
                "import、digest</a>——で、どちらの向きにも動きます。"
            ),
        },
        {
            "t": "steps",
            "id": "start",
            "tone": "deep",
            "h2": "2 分",
            "intro": "",
            "items": [
                {
                    "title": 'インストール',
                    "body": 'バイナリ 1 つです。ランタイムも、解決を待つ依存もありません。',
                    "code": 'cargo install kevy\nkevy --port 6379',
                },
                {
                    "title": 'クライアントを向ける',
                    "body": (
                        "いま使っているものが、そのまま動きます——kevy製のクライアントを"
                        "入れる必要はありません。<b>node-redis</b>／<b>ioredis</b>、"
                        "<b>go-redis</b>、<b>StackExchange.Redis</b>、<b>redis-py</b>、"
                        "<b>hiredis</b>がそのまま繋がり、kevy独自の動詞は同じクライアントの"
                        "rawコマンド経路から届きます。この6つは、pushのたびにCIで実サーバーへ"
                        "同一の梯子を流しています。"
                        "<a href=\"/docs/clients/\">言語ごとの例（英語）</a>。"
                    ),
                    "code": 'redis-cli -p 6379\n> SET greeting hello\nOK\n> TTL greeting\n(integer) -1',
                },
                {
                    "title": 'Redis にできないことをする',
                    "body": 'インデックスを宣言すれば、書き込みの側が最新に保ちます。',
                    "code": 'IDX.CREATE idx:city ON PREFIX user: FIELD city TYPE str KIND range\nIDX.QUERY  idx:city EQ osaka',
                },
            ],
        },
        {
            "t": "callout",
            "kind": "loss",
            "title": "kevy がやらないこと",
            "body": (
                "<b>クラスタではありません。</b>レプリケーションとフェイルオーバーは"
                "ありますが、マシンをまたぐシャーディングはなく、今後もありません。"
                "<b>AUTH も TLS もありません</b>——プライベートなネットワークで動かす"
                "か、それらを正しく処理するものの後ろに置いてください。<b>複数キーの"
                "書き込みは shard 単位でのみ原子的で、全体では原子的ではありません</b>"
                "——shard をまたぐ <code>RENAME</code> や <code>MSET</code> は、1 つの"
                "原子的な操作にはなりません。<a href=\"~/docs/commands/\">差異は"
                "すべて、コマンドごとに文書化してあります</a>。"
                "<a href=\"~/choose/\">そして、そもそも使うべきでない場合は"
                "こちら。</a>"
            ),
        },
    ],
}

# ── /migrate/ ───────────────────────────────────────────────────────────────

PAGES["migrate"] = {
    "title": "Redis やデータベースから移る — kevy",
    "desc": "Redis や Postgres から kevy に移る理由、何が変わるのか、何を差し出すことになるのか、そして書き直しなしで移る方法。",
    "foot": "何が変わり、何を差し出すのか",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "移行",
            "h1": "なぜ移るのか、<br>そして何を差し出すのか",
            "lede": (
                "話は 2 つあります。<b>Redis</b> から来る場合、プロトコルは同じなので、"
                "問うべきは「何の挙動が違うのか」です。<b>リレーショナルデータベース</b>"
                "から来る場合、同じものは何ひとつないので、問うべきは「ワークロードの"
                "どの部分を移すべきか」です。答えは<b>その一部</b>であり、それがどこ"
                "なのかを、これから示します。"
            ),
        },
        {
            "t": "prose",
            "h2": "Redis から移る",
            "body": [
                "<b>クライアントは変わりません。</b>kevy は RESP2 と RESP3 を話し、"
                "188 個のコマンドに応答します。既存のライブラリの接続先を変えるだけで、"
                "コードもそのまま、redis-cli もそのままです。新しく覚える SDK も"
                "プロトコルもありません。",
                "<b>だから本当の問題は、何が得られるのかだけです。</b>得られるものは "
                "4 つあります。そのどれにも価値を感じないなら、Redis に留まって"
                "ください。あれは見事なソフトウェアであり、乗り換えのための乗り換えは "
                "1 週間の浪費です。",
            ],
        },
        {
            "t": "steps",
            "h2": "具体的に、何が得られるのか",
            "intro": "",
            "items": [
                {
                    "title": "Redis が動けない場所で動く",
                    "body": "バイナリに組み込み、ブラウザのタブに配り、アロケータのない Cortex-M で起動できます。いまはそのひとつひとつに、独自の API を持つ専用のストレージ層が要ります。ここでは同じエンジン、同じコマンドです。クライアント側に 2 つ目のキャッシュを書いた経験があるなら、それが検討する理由です。",
                },
                {
                    "title": "検索サービスも置き換えられる",
                    "body": "セカンダリインデックス、マテリアライズドビュー、ベクトル KNN、BM25 全文検索がエンジンに入っています。モジュールでもサイドカーでもなく、元データからずれていく 2 つ目のコピーでもありません。Redis と検索クラスタを併用しているチームは、多くの場合ひとつにまとめられます。",
                    "code": (
                        "# look up by a field, not just by the key\n"
                        "IDX.CREATE idx:city ON PREFIX user: FIELD city TYPE str KIND range\n"
                        "IDX.QUERY  idx:city EQ osaka\n"
                        "\n"
                        "# vectors, in the same engine, over the same keys\n"
                        "IDX.CREATE idx:sem ON PREFIX doc: FIELD vec TYPE vector KIND ann DIM 768 DISTANCE cosine\n"
                        "IDX.QUERY  idx:sem KNN \"<vector>\" LIMIT 10"
                    ),
                },
                {
                    "title": "すでに使っている操作が、そのまま速くなる",
                    "body": "同じマシンで Redis 8 に対して、GET は 1.4 倍、SET は 2.7 倍、INCR は 1.8 倍です。ただし、当てにする前に表の全体を読んでください——LPUSH と ZADD は 12% と 10% しか上回っておらず、リストやソート済みセットがホットパスなら、これは移る理由になりません。",
                },
                {
                    "title": "データセットが RAM に収まる必要が、もうない",
                    "body": "ストアに RAM 予算を与えると、最もコールドな値はディスク上の使い捨て value log へ退避し、アクセスされたときに戻ります——コールドキーの上でもすべてのコマンドは変わらず、追記専用ログの永続化コントラクトも無変更です。RAM が保持できるキーの数を、ディスクがデータの量を決めます。大きな値やロングテールのために「Redis と別のディスクストア」を並べる構成を、これがひとつにします。正直な限界も述べます。既定ではオフで、v1 で退避するのは文字列とハッシュ(リスト、セット、ストリームはホットのまま)、そして 64 バイト未満の値は決して退避されません——stub が値と同じ大きさになるからです。",
                    "code": "# kevy.toml\n[tiering]\nbudget = \"70%\"      # or \"4gb\", or \"auto\"",
                },
            ],
        },
        {
            "t": "callout",
            "kind": "loss",
            "title": "Redis を離れることで失うもの",
            "body": (
                "<b>クラスタはありません。</b>レプリカはコピーであって、シャードでは"
                "ありません。<b>AUTH も TLS もありません。</b>そして<b>いくつかの"
                "コマンドは挙動が違います</b>——知っておくべき筆頭は、シャードをまたぐ "
                "<code>RENAME</code> が原子的ではないこと(複数キーの書き込みは "
                "shard 単位でのみ原子的)です。どれもバグではなく、"
                "すべてコマンドごとに文書化してあります。移ると決めたあとではなく、"
                "決める前に読んでください——<a href=\"~/docs/commands/\">各コマンドの"
                "本当のコストと、本当の差異</a>。"
            ),
        },
        {
            "t": "code",
            "h2": "移行の手順——Redis から",
            "caption": "Redis から export し、kevy に import して、両者が一致することを確かめます。以下のコマンドは、すべて実行したものです。",
            "text": '# 1. dump what you want to move. it is a RESP file — readable,\n#    diffable, and it streams rather than loading into memory.\nkevy-cli export -p 6379 --prefix user: dump.resp\n-> exported 41023 keys -> dump.resp\n\n# 2. load it. --strict stops on the first error rather than\n#    limping onward with a half-migrated keyspace.\nkevy-cli import -p 6380 --strict dump.resp\n-> imported 82046 ok, 0 errors, offset 4108331\n\n# 3. prove they agree, rather than hoping.\nkevy-cli digest -p 6379 user:\nkevy-cli digest -p 6380 user:\n-> 41023 keys 3bca92aa52269300     # the same hash, or you did not migrate\n\n# an interrupted import resumes where it stopped:\nkevy-cli import -p 6380 --resume dump.resp',
        },
        {
            "t": "prose",
            "h2": "リレーショナルデータベースから移る",
            "body": [
                "<b>データベースを移してはいけません。</b>移すのは、そもそも"
                "データベースの問題ではなかった部分です。",
                "セッション。レート制限。フィーチャーフラグ。ジョブキュー。すべての"
                "リクエストが読み、誰も結合しないホットな行。たいていのアプリケーション"
                "でこれらは Postgres の中にあり、叩かれ続けているのもそこです。"
                "リレーショナルデータベースがそれらを苦手だからではありません。"
                "それらが、そもそも問い合わせではなかったからです。ただの参照です。"
                "キーは、もう分かっています。",
                "<b>Postgres には、他に並ぶもののない仕事を残してください</b>——結合、"
                "アドホックなクエリ、分析、無関係な行にまたがる本物の分離レベルを"
                "伴うトランザクション。kevy は配信の経路を引き受け、データベースに"
                "夜を返します。",
                "<b>そして、単一テーブルの配信用の読み取りも移せます。</b>"
                "<code>TABLE.DECLARE</code> で型付きカラム、セカンダリインデックス、"
                "複合 <code>ORDER BY</code> パスを一度だけ宣言する——あるいは手元の "
                "PG/MySQL スキーマファイルを <code>kevy-sql</code> でコンパイルする——"
                "それだけで、1 つのテーブルの読み取りパス(インデックスされた WHERE、"
                "残りのフィルタ、ORDER BY、ページング、COUNT)が kevy のインデックスに"
                "コンパイルされます。クエリ時のプランナはありません。kevy-sql は"
                "ビルド時のコンパイラであって SQL エンジンではありません。join と"
                "アドホック SQL は名前つきで拒否され、Postgres に残ります。ほとんどの"
                "アプリケーションが ORM に実際に求めているのは、その部分です。",
            ],
        },
        {
            "t": "table",
            "h2": "どの部分を移すべきか",
            "intro": "ワークロードごとに。赤い 3 行が、よく間違われるところです。",
            "head": ["ワークロード", "移すべきか", "理由"],
            "rows": [
                ["セッション、トークン", "*はい", "TTL つきの、キーによる参照です。データベースは仕事としてではなく、厚意でやってくれていました。"],
                ["レート制限、カウンタ", "*はい", "期限つきの INCR は原子的で O(1) です。SQL では、最もホットな行に対する行ロックになります。"],
                ["ジョブキュー", "*はい", "リストとストリーム。コンシューマグループと、メッセージごとの確認応答があります。キュー用のテーブルは、手数の増えたロックの慣習にすぎません。"],
                ["フィーチャーフラグ、設定", "*はい", "絶えず読まれ、めったに書かれず、結合されることはありません。"],
                ["単一テーブルの読み取り(絞り込み、順序、ページング)", "*はい", "テーブルのアクセスパスを一度宣言する——あるいはスキーマファイルを <code>kevy-sql</code> でコンパイルする——だけで、インデックスされた WHERE + ORDER BY + LIMIT の読み取りは参照のままです。<a href=\"~/use/app-store/\">読みを捌く</a>を参照してください。"],
                ["集計(件数や合計)", "*多くの場合", "マテリアライズドビューが書き込みの側で最新に保つので、読むたびに計算し直す必要がありません。"],
                ["複数テーブルにまたがる結合", "!移すな", "kevy に結合はなく、今後も持ちません。それは Postgres の仕事です。"],
                ["分析、アドホックなクエリ", "!移すな", "クエリプランナもオプティマイザもありません。試さないでください。"],
                ["無関係な行にまたがるトランザクション", "!移すな", "MULTI はシャード単位であって、全体ではありません。キースペース全体で直列化可能な分離が要るなら、必要なのはデータベースです。"],
            ],
            "note": (
                "赤い 3 行は、やることリストではありません。拒否です——kevy は結合も"
                "オプティマイザも持ちません。どちらも中途半端にやるくらいなら、"
                "やらないほうがましだからです。<a href=\"~/docs/rds-workloads/\">"
                "リレーショナルなワークロードのすべてと、ここでの実際のコスト</a>を"
                "書いてあります。正直な答えが「Postgres に置いたままにしてください」"
                "になるものも含めて。"
            ),
        },
        {
            "t": "code",
            "h2": "移行の手順——データベースから",
            "caption": "一度に切り替えることはしません。ワークロードをひとつ移し、データベースを正としたまま、測ってください。",
            "text": "# 1. pick ONE workload. sessions are the usual first, because\n#    nothing joins against them and losing one is survivable.\n\n# 2. write to both for a week. reads still come from Postgres.\n#    you are checking that the shapes match, not that it is fast.\n\n# 3. flip reads to kevy. keep the dual write.\nredis-cli SET session:$SID \"$JSON\" EX 3600\n\n# 4. when it has been boring for a fortnight, drop the table.\n\n# then do the next workload. rate limits, then queues, then\n# whichever of your read paths a secondary index can answer.",
        },
        {
            "t": "callout",
            "kind": "note",
            "title": "また出ていきたくなったら",
            "body": (
                "同じ 3 つのコマンドが、逆向きにも使えます。"
                "<code>kevy-cli export</code> はプレーンな RESP ファイルを書き出し、"
                "Redis 互換のサーバーなら、どれでもそれを取り込めます。そして "
                "<code>digest</code> が、コピーが忠実であることを証明します。"
                "<a href=\"~/docs/migration/\">移行ガイドは、入ってくる手順と同じ"
                "丁寧さで、出ていく手順も扱っています</a>——動けなくなって留まられる"
                "より、きれいに出ていってもらうほうが、私たちとしてもずっといいと"
                "考えています。"
            ),
        },
    ],
}

# ── /choose/ ────────────────────────────────────────────────────────────────

PAGES["choose"] = {
    "title": "kevy を使うべきか — kevy",
    "desc": "自分の問題にどの kevy が合うのか、選ぶことで何を差し出すのか、そして代わりに別のものを使うべき場合。",
    "foot": "答えが「いいえ」になる場合も含めて",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "選ぶ",
            "h1": "kevy を使うべきでしょうか",
            "lede": (
                "使うべきでない場合もあります。ここでは、実際に判断する順番どおりに"
                "並べます。<b>そもそもキーバリューという形が正しいのか。データはどこに"
                "置かれる必要があるのか。そして、何を差し出すことになるのか。</b>"
            ),
        },
        {
            "t": "prose",
            "h2": "まず——キーバリューという形は、正しいか",
            "body": [
                "<b>キーが分かっているなら、kevy を使ってください。</b>セッション ID、"
                "ユーザー ID、キューの名前、キャッシュキー。読み取りは問い合わせでは"
                "なく参照です。これは思われているよりも広い範囲を覆います。さらに"
                "セカンダリインデックスとマテリアライズドビューが、問い合わせのいくつか"
                "も参照に変えます。TABLE レイヤーはその先へ行きます。型付きカラムと"
                "インデックスを一度宣言すれば(あるいは PG/MySQL のスキーマファイルを "
                "<code>kevy-sql</code> でコンパイルすれば)、単一テーブルの読み取り"
                "パス——インデックスされた WHERE、残りのフィルタ、ORDER BY、ページング"
                "——は参照のままです。",
                "<b>読み取りが本当にクエリなら、kevy を使わないでください。</b>5 つの"
                "テーブルにまたがる結合、アドホックな分析、無関係な行にまたがり本物の"
                "分離レベルを要求するトランザクション——それは PostgreSQL の仕事であり、"
                "PostgreSQL に留めておくべきです。<a href=\"~/docs/rds-workloads/\">"
                "リレーショナルな各ワークロードが、ここでいくらかかるか</a>を書いて"
                "あります。答えが「やめておきなさい」になるものも含めて。",
                "<b>1 台で足りないなら、kevy を使わないでください。</b>クラスタモード"
                "はなく、今後もできません。1 台の kevy は毎秒数百万回の操作をこなし、"
                "RAM 予算を与えればデータセットは RAM より大きくできます(コールドな値は"
                "ディスクへ退避します——RAM がキー数を、ディスクがデータ量を決めます)。"
                "それでも 1 台のスループットを超えたら、シャーディングするものが必要です。"
                "kevy は、それではありません。",
            ],
        },
        {
            "t": "table",
            "h2": "次に——データは、どこに置かれる必要があるか",
            "intro": "これが形を決めます。どの行でも、コマンドは同じです。",
            "head": ["状況", "選ぶもの", "理由"],
            "rows": [
                ["複数のサービスがデータを共有する", "サーバー",
                 "1 プロセス、ポートで RESP。既存の Redis クライアントが、そのまま接続できます。"],
                ["1 つのプログラムがデータを所有する", "組み込み",
                 "ソケットも、2 つ目のプロセスも、シリアライズも要りません。ラウンドトリップではなく、関数呼び出しです。"],
                ["データはユーザーの端末のもの", "ブラウザ",
                 "481 KB の WebAssembly。本物の TTL と pub/sub があり、ブラウザのファイルシステムに永続化されます。オフラインでも動きます。"],
                ["リクエストごとに、エッジで動く", "エッジ",
                 "暖機するものも、張りにいく接続もありません。ストアは、コードと同じ isolate の中にあります。"],
                ["OS もヒープもないデバイス", "ベアメタル",
                 "kevy-store は no_std です。固定 arena のみ、アロケータなし。CI が push のたびに Cortex-M で起動させています。"],
            ],
            "note": (
                "選んだら固定される、というものではありません。組み込み API とワイヤ"
                "プロトコルは同じ操作を公開しているので、プロセス内ストアで手狭に"
                "なったプログラムは、データベースの開き方を変えるだけでサーバーに"
                "移れます。使い方を書き直す必要はありません。"
            ),
        },
        {
            "t": "faq",
            "h2": "最後に——何を差し出すことになるか",
            "items": [
                {
                    "q": "本当に Redis のドロップイン置き換えになりますか",
                    "a": "ワイヤの上では、なります。RESP2 と RESP3、188 個のコマンドに対応し、クライアントライブラリは違いに気づきません。挙動もおおむね同じですが、その例外こそが要点です。シャードをまたぐ <code>RENAME</code> は原子的ではありません——複数キーの書き込みは shard 単位でのみ原子的です。また SCAN のカーソルは発行したサーバーでのみ有効で、これは Redis Cluster のノード単位の性質と同じです。<a href=\"~/docs/commands/\">188 個すべてのコマンドに、本当の差異と本当のコストを併記してあります</a>。Redis の文書から書き写したものではなく、実装から読み出したものです。",
                },
                {
                    "q": "データセットは RAM に収まっている必要がありますか",
                    "a": "もう、ありません。ティアリングをオンにして、ストアに RAM 予算を与えてください。最もコールドな値はディスク上の使い捨て value log へ退避し、アクセスされたときに戻ります。コールドキーの上でもすべてのコマンドは厳密に同じ意味を保ち、追記専用ログの永続化コントラクトは無変更です——RAM が保持できるキーの数を、ディスクがデータの量を決めます。正直な限界も述べます。既定ではオフで、v1 で退避するのは文字列とハッシュ(リスト、セット、ソート済みセット、ストリームはホットのまま)、64 バイト未満の値は決して退避されません。<a href=\"~/docs/tiering/\">ティアリングのガイド</a>に、どの数字が実測で、どれがベンチマシンの実行待ちのターゲットなのかを明記してあります。",
                },
                {
                    "q": "マシンが落ちたら、どうなりますか",
                    "a": "書き込みはすべて、まず追記専用ログに入り、起動時にログが再生されます。既定の <code>everysec</code> の fsync なら、強制終了で失うのは最大 1 秒分の書き込みです。<code>appendfsync = \"always\"</code> にすれば失うものはありませんが、スループットを代償に払います。スナップショットは、再生にかかる時間を抑えるためだけに存在します。<a href=\"~/docs/persistence/\">永続化のガイド</a>に数字があります。",
                },
                {
                    "q": "マシン障害を生き延びられますか",
                    "a": "はい。プライマリ 1 台とレプリカ N 台で、本物のフェイルオーバーがあります。計画的な引き継ぎ、エポックによるフェンシングを伴うクラッシュ時の選出、そして任意で選べる一貫性の段階(<code>WAIT</code>、read-your-writes トークン、上限つきの遅延)。<b>得られない</b>のは、マシンをまたぐデータのシャーディングです。レプリカはコピーであって、切片ではありません。<a href=\"~/docs/availability/\">可用性のガイド</a>に、フェイルオーバーでどの書き込みが生き残り、どれが生き残らないのかを明記してあります。",
                },
                {
                    "q": "認証はありますか",
                    "a": "ありません。今後もありません。AUTH も ACL も TLS も、恒久的に対象外です。kevy はプライベートなネットワークで動かすか、それらを正しく行うプロキシの後ろに置いてください。中途半端な認証層は、正直に何もないことよりも悪いものです。人に信頼させてしまうからです。",
                },
                {
                    "q": "手狭になったら、あるいは気が変わったら",
                    "a": "<code>kevy-cli export</code> がキースペースをプレーンな RESP ファイルに書き出し、Redis 互換のサーバーなら、どれでもそれを取り込めます。そして <code>kevy-cli digest</code> が、何かを捨ててしまう前に、コピーが忠実であることを証明します。<a href=\"~/docs/migration/\">移行ガイド</a>は、入ってくる手順と同じ丁寧さで、出ていく手順も扱っています。",
                },
            ],
        },
        {
            "t": "callout",
            "kind": "note",
            "title": "まだ決められませんか",
            "body": (
                "<a href=\"~/play/\">playground</a> を開いてください。WebAssembly に"
                "コンパイルされた本物の kevy エンジンが、あなたのタブの中で動いて"
                "います。キーを書き、TTL が切れていく様子を眺め、自分のディスクに"
                "置かれた追記専用ログを覗いてみてください。録画ではありませんし、"
                "サーバーも介在しません。"
            ),
        },
    ],
}

# ── /use/cache/ ─────────────────────────────────────────────────────────────

PAGES["use/cache"] = {
    "title": "キャッシュとセッション — kevy",
    "desc": "kevy でのセッション、ホットな行、レート制限、フィーチャーフラグ——タスクと、そのためのコマンドと、それぞれのコスト。",
    "foot": "ほとんどのチームが、最初に移すワークロード",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "キャッシュとセッション",
            "h1": "データベースが嫌がる負荷を引き取る",
            "lede": (
                "セッション、レート制限、フィーチャーフラグ、すべてのリクエストが読む"
                "ホットな行。たいていのアプリケーションでこれらは Postgres の中にあり、"
                "叩かれ続けているのもそこです。データベースがそれらを苦手だからでは"
                "ありません。<b>そもそも問い合わせではなかったからです。キーは、もう"
                "分かっています。</b>"
            ),
        },
        {
            "t": "prose",
            "h2": "なぜ向いているのか",
            "body": [
                "どれも同じ形をしています。すでに手元にあるキー、小さな値、そして"
                "寿命。kevy は参照を O(1) で行い、cron を使わずキー自身を期限切れに"
                "し、1 台のマシンで毎秒数百万回それをこなします。",
                "見落とされがちなのが<b>期限切れ</b>です。データベースの上に作った"
                "キャッシュには掃除役が要り、バグはその掃除役に棲みつきます。ここでは"
                "エンジンが、誰かに読まれるかどうかに関係なく、時間の来たキーを"
                "落とします。以下の 4 つのタスクは、どれも動いている kevy に対して、"
                "<code>redis-cli</code> からそのまま貼り付けられます。",
                "<b>そして、ロングテールは RAM を超えられます。</b>RAM 予算"
                "(<code>[tiering]</code>)をオンにすると、最もコールドな値は使い捨ての"
                "ディスクログへ退避し、アクセスされたときに戻ります——コールドキーでも"
                "コマンドは変わらず、永続化も無変更です。めったに読まれないセッションや"
                "アーカイブが、2 つ目のストアなしで RAM を占めなくなります。既定ではオフ"
                "で、v1 で退避するのは文字列とハッシュです。正直な限界は"
                "<a href=\"~/docs/tiering/\">ティアリングのガイド</a>にあります。",
            ],
        },
        {
            "t": "recipe",
            "h2": "ひとりでに片付くセッション",
            "goal": "セッションごとにキーは 1 つ。最後に触れてから 1 時間で、ひとりでに消えます——掃除役も、cron も、期限切れ行のテーブルも要りません。",
            "cost_t": "コストと制約",
            "items": [
                {
                    "do": "セッションを、寿命つきで書く",
                    "code": """SET session:7f3a '{"user":"ada","role":"admin"}' EX 3600
-> OK""",
                },
                {
                    "do": "リクエストのたびに読む",
                    "code": """GET session:7f3a
-> "{\\"user\\":\\"ada\\",\\"role\\":\\"admin\\"}"
TTL session:7f3a
-> (integer) 3599""",
                },
                {
                    "do": "スライド式の期限——アクティビティで更新する",
                    "note": "触れるのは値ではなく時計です——セッションが消えるのは、ログインの 1 時間後ではなく、ユーザーが静かになってから 1 時間後です。",
                    "code": """EXPIRE session:7f3a 3600
-> (integer) 1""",
                },
            ],
            "cost": (
                "セッションはキー 1 つなので、ここの手順はすべて O(1) で原子的です。"
                "ひとりのユーザーの状態を複数のキーに広げると、原子性はシャードの"
                "境界で終わります——キーに <code>{hashtag}</code> を入れて、同じ場所に"
                "寄せてください。"
            ),
        },
        {
            "t": "recipe",
            "h2": "エンドポイントにレート制限をかける",
            "goal": "クライアントごと、ウィンドウごとにカウンタ 1 つ。1 分の中の 101 回目のリクエストは、断られます。",
            "cost_t": "コストと制約",
            "items": [
                {
                    "do": "リクエストを数える",
                    "code": """INCR rate:203.0.113.7
-> (integer) 1""",
                },
                {
                    "do": "最初の 1 回で、ウィンドウを開く",
                    "note": "返答が 1 だったときだけです——以降のリクエストは、すでに開いているウィンドウに乗ります。",
                    "code": """EXPIRE rate:203.0.113.7 60
-> (integer) 1""",
                },
                {
                    "do": "上限を超えたら、断る",
                    "note": "カウンタが上限を超えたら、ハンドラで 429 を返します。ウィンドウは、そのまま数え続けます。",
                    "code": """INCR rate:203.0.113.7
-> (integer) 2      (the window survives)""",
                },
            ],
            "cost": (
                "これは<b>固定</b>ウィンドウであって、スライド式ではありません。"
                "境界をまたぐバーストは、短い区間では上限の 2 倍まで通りえます。"
                "不正利用の抑止ならそれで十分ですし、全体が O(1) のコマンド 2 つで"
                "済みます。もっと滑らかな整形が要るなら、新しいシステムではなく、"
                "キーの数で払ってください。"
            ),
        },
        {
            "t": "recipe",
            "h2": "フィーチャーフラグ——すべてのリクエストが読む",
            "goal": "全フラグをひとつのハッシュに。ホットパスは O(1) の読み取り 1 回、フラグを全員ぶん切り替えるのは書き込み 1 回です。",
            "cost_t": "コストと制約",
            "items": [
                {
                    "do": "フラグを設定する",
                    "code": """HSET flags new-checkout on dark-mode on beta-search off
-> (integer) 3""",
                },
                {
                    "do": "ホットパスで 1 つ読む",
                    "code": """HGET flags new-checkout
-> "on\"""",
                },
                {
                    "do": "1 つを、全員に対して、いま切り替える",
                    "code": """HSET flags beta-search on
-> (integer) 0      (0 = updated, not added)
HGETALL flags""",
                },
            ],
            "cost": (
                "ひとつのハッシュはひとつのシャードに載るので、フラグの読み取りには"
                "すべて同じシャードが答えます。フラグ程度の読み取り頻度なら、それでも"
                "毎秒数百万回です。そこがホットスポットになったら、ハッシュを画面や"
                "チームの単位で分けてください。"
            ),
        },
        {
            "t": "recipe",
            "h2": "データベースが所有する行をキャッシュする",
            "goal": "ホットな行はメモリから返します。真実は Postgres のまま、コピーが保険より長生きすることはありません。",
            "cost_t": "コストと制約",
            "items": [
                {
                    "do": "読み取りミスのとき、保険の TTL つきで埋める",
                    "code": """SET user:881 "$json" EX 300
-> OK""",
                },
                {
                    "do": "読み取りは、コピーから返す",
                    "code": """GET user:881""",
                },
                {
                    "do": "書き込んだら無効化する——タイマーを待たない",
                    "note": "データベースへの書き込みがコミットしてから削除します。次の読み取りはミスして埋め直し、正しい値に戻ります。",
                    "code": """DEL user:881
-> (integer) 1""",
                },
            ],
            "cost": (
                "キャッシュは真実の 2 つ目のコピーであり、間違うことがあります——"
                "それを解決できるものは、ありません。<b>タイマーではなく、書き込みで"
                "無効化してください</b>。TTL は計画ではなく、最後の保険です。複数キー"
                "の <code>DEL</code> や <code>MSET</code> が原子的なのは、ひとつの"
                "シャードの中だけです。2 つのキーが必ず一緒に変わらなければならない"
                "なら、<code>{hashtag}</code> で同じ場所に寄せてください。"
            ),
        },
        {
            "t": "cards",
            "h2": "次に",
            "intro": "",
            "items": [
                {"kicker": "ガイド", "title": "クックブック", "body": "セッション、レート制限、リーダーボード、フィードの実用レシピ。", "go": "読む", "href": "docs/cookbook/"},
                {"kicker": "ガイド", "title": "永続化", "body": "kill -9 で何が残り、fsync の方針が何を代償にするのか。", "go": "読む", "href": "docs/persistence/"},
                {"kicker": "リファレンス", "title": "全コマンド", "body": "188 個のコマンド。それぞれの本当のコストと、Redis との差異つき。", "go": "調べる", "href": "docs/commands/"},
            ],
        },
    ],
}

# ── /use/queue/ ─────────────────────────────────────────────────────────────

PAGES["use/queue"] = {
    "title": "キューとバックグラウンドジョブ — kevy",
    "desc": "kevy でのジョブキュー。単純な仕事にはリスト、失うわけにいかない仕事にはコンシューマグループつきのストリーム。",
    "foot": "ワーカーが落ちても、ジョブを失わないキュー",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "キューとジョブ",
            "h1": "仕事をワーカーに渡し、<br>落ちたら取り返す",
            "lede": (
                "リレーショナルデータベースのキュー用テーブルは、手数の増えたロックの"
                "慣習にすぎません。kevy には本物のキューが 2 つあります。ジョブを"
                "失っても許されるなら<b>リスト</b>、許されないなら<b>コンシューマ"
                "グループつきのストリーム</b>です。"
            ),
        },
        {
            "t": "prose",
            "h2": "2 つの、どちらを使うか",
            "body": [
                "<b>リストを使う</b>のは、やり直しが安く、ワーカーが作業中に落ちる"
                "見込みが低いときです——メールの送信、キャッシュの暖機、CDN パスの"
                "無効化。<code>BRPOP</code> は仕事が来るまでブロックするので、ワーカー"
                "はポーリングしません。",
                "<b>ストリームを使う</b>のは、ジョブを失ってはならないときです。"
                "コンシューマグループは各メッセージを、ちょうど 1 つのワーカーに渡し、"
                "渡したことを覚えています。ワーカーが確認応答の前に落ちても、"
                "メッセージは保留リストに残り、別のワーカーが引き取れます——"
                "ストリームが存在する理由はまさにそれであり、それが、キューと願望"
                "との違いです。",
            ],
        },
        {
            "t": "recipe",
            "h2": "やり直せる仕事には、リスト",
            "goal": "プロデューサが積み、ブロックしていたワーカーが、仕事の来た瞬間に目を覚まします。コマンド 2 つ。ポーリングのループも、スケジューラもありません。",
            "cost_t": "コストと制約",
            "items": [
                {
                    "do": "プロデューサ——ジョブを積む",
                    "code": """LPUSH jobs:email '{"to":"ada@example.com","tpl":"welcome"}'
-> (integer) 1""",
                },
                {
                    "do": "ワーカー——来るまでブロックする",
                    "note": "ポーリングも、sleep も、殺到もありません——pop はジョブが届いた瞬間に返り、何もなければ 30 秒で手ぶらのまま返ります。",
                    "code": """BRPOP jobs:email 30
-> 1) "jobs:email"
   2) "{\\"to\\":\\"ada@example.com\\",\\"tpl\\":\\"welcome\\"}\"""",
                },
                {
                    "do": "遅延ジョブ——スコアが期日",
                    "note": "ZPOPMIN.BELOW は kevy 独自のコマンドです。実際に期日の来たものだけを取り、まだのジョブが現れたところで止まります。",
                    "code": """ZADD jobs:due 1783875499 '{"id":"j-91"}'
-> (integer) 1
ZPOPMIN.BELOW jobs:due 1783875500
-> the job payload, only if it is due""",
                },
            ],
            "cost": (
                "<b>pop したジョブは、そのワーカーがやり切らなければ、失われます。</b>"
                "それがコマンド 2 つで済むことの代償です——やり直せる仕事にだけ、"
                "この取引を選んでください。またマルチシャードのサーバーでは、複数の"
                "キーにまたがる <code>BLPOP</code> は、Redis の厳密な左から右への"
                "優先順を守りません。接続自身のシャードにあるキーが、先に処理され"
                "ます。"
            ),
        },
        {
            "t": "recipe",
            "h2": "失えない仕事には、ストリーム",
            "goal": "各ジョブはちょうど 1 つのワーカーに渡り、確認応答があるまで保留のままです。落ちたワーカーのジョブは、履歴をそっくり残したまま引き取れます。",
            "cost_t": "コストと制約",
            "items": [
                {
                    "do": "セットアップ時に一度——グループを作る",
                    "code": """XGROUP CREATE jobs:pay g1 $ MKSTREAM
-> OK""",
                },
                {
                    "do": "プロデューサ——ジョブを追記する",
                    "code": """XADD jobs:pay * order 4410 amount 8400
-> "1783875499458-0\"""",
                },
                {
                    "do": "ワーカー——読んで、働いて、確認応答する",
                    "note": "確認応答する ID は、XREADGROUP が渡してきたものです。XACK までジョブは保留のまま——あなたの担当として、記録に残っています。",
                    "code": """XREADGROUP GROUP g1 worker-3 COUNT 1 BLOCK 5000 STREAMS jobs:pay >
XACK jobs:pay g1 1783875499458-0""",
                },
                {
                    "do": "ワーカーが XACK の前に落ちた——ジョブを引き取る",
                    "code": """XAUTOCLAIM jobs:pay g1 worker-7 60000 0-0
# claims anything idle for more than 60 s

XPENDING jobs:pay g1
# what is still outstanding, and who has it""",
                },
            ],
            "cost": (
                "<b>ストリームは、ただではありません。</b><code>MAXLEN</code> による"
                "切り詰めはストリームの重みを計算し直すため、ストリーム全体に対して "
                "O(N) です——<code>XADD</code> のたびではなく、定期的に切り詰めて"
                "ください。また <code>XREADGROUP</code> の <code>COUNT</code> が制限"
                "するのは渡される量であって、<b>走査される量ではありません</b>。"
                "未配信の末尾は、まず全体が実体化されます。コマンドごとの詳細は"
                "<a href=\"~/docs/commands/\">リファレンス</a>にあります。"
            ),
        },
        {
            "t": "cards",
            "h2": "次に",
            "intro": "",
            "items": [
                {"kicker": "ガイド", "title": "クックブック", "body": "リトライやデッドレターのパターンを含む、キューのレシピ。", "go": "読む", "href": "docs/cookbook/"},
                {"kicker": "リファレンス", "title": "ストリームのコマンド", "body": "XADD、XREADGROUP、XAUTOCLAIM ほか。実際のコストつき。", "go": "調べる", "href": "docs/commands/"},
            ],
        },
    ],
}

# ── /use/realtime/ ──────────────────────────────────────────────────────────

PAGES["use/realtime"] = {
    "title": "リアルタイムと pub/sub — kevy",
    "desc": "kevy の pub/sub によるチャット、プレゼンス、通知、ライブダッシュボード。追いつけない購読者に何が起きるのかも含めて。",
    "foot": "ファンアウトと、保証しないこと",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "リアルタイム",
            "h1": "聞いている全員に、<br>そのまま届ける",
            "lede": (
                "チャット、プレゼンス、通知、ひとりでに更新されるダッシュボード。"
                "1 回の publish で多数の購読者へ、ポーリングなしで届きます。"
                "<b>そしてブラウザ向けビルドでは、同じ pub/sub がサーバーなしで、"
                "2 つのタブの間で動きます。</b>"
            ),
        },
        {
            "t": "prose",
            "h2": "なぜ向いているのか——そして、どこで向いていないのか",
            "body": [
                "pub/sub は投げっぱなしです。メッセージは<b>その瞬間に</b>購読して"
                "いる相手へ届きます。1 秒後に接続した人が受け取ることは決してなく、"
                "確認応答もありません。プレゼンスの ping やライブなカウンタには"
                "まさに正しく、失って困るものには、まさに間違っています。",
                "<b>メッセージを失うことが問題なら、代わりにストリームを使って"
                "ください</b>——<a href=\"~/use/queue/\">キュー</a>を参照してください。"
                "ストリームは履歴を保持し、コンシューマグループに対応し、オフライン"
                "だったクライアントが追いつけます。pub/sub は安い手段であり、その"
                "安さが、取引の条件です。",
            ],
        },
        {
            "t": "recipe",
            "h2": "聞いている全員へ、メッセージをファンアウトする",
            "goal": "1 回の publish が、その瞬間につながっているすべての購読者に届きます——チャットルーム、通知、ライブなカウンタ。",
            "cost_t": "コストと制約",
            "items": [
                {
                    "do": "各クライアントが購読する",
                    "note": "PSUBSCRIBE なら、チャネルの一族まるごとを 1 本の接続で受け取れます。",
                    "code": """SUBSCRIBE room:42
PSUBSCRIBE room:*          # every room, one connection""",
                },
                {
                    "do": "publish する——返答が聴衆の数",
                    "code": """PUBLISH room:42 '{"user":"ada","text":"hello"}'
-> (integer) 3             # how many subscribers received it""",
                },
            ],
            "cost": (
                "<b>遅い購読者は、いつまでもバッファされるのではなく、切り捨てられ"
                "ます。</b>クライアントが追いつけない場合、そのメッセージは、サーバー"
                "のメモリを際限なく増やす代わりに破棄されます。意図した選択であり、"
                "配信を当てにする前に知っておくべきことです。確認応答も、再送も"
                "ありません——どちらかが必要なら、必要なのはチャネルではなくストリーム"
                "です。<a href=\"~/docs/pubsub/\">pub/sub のガイド</a>に、限界を"
                "具体的に書いてあります。"
            ),
        },
        {
            "t": "recipe",
            "h2": "プレゼンス——いま誰がオンラインか",
            "goal": "帳簿づけはエンジンの期限切れに任せます。静かになったクライアントは、ひとりでに名簿から落ちます。",
            "cost_t": "コストと制約",
            "items": [
                {
                    "do": "ハートビート——寿命つきのキー",
                    "note": "クライアントは 10 秒ごとに更新します。更新をやめた者から、期限が切れます。",
                    "code": """SET presence:ada online EX 30
-> OK""",
                },
                {
                    "do": "名簿は、セットで",
                    "code": """SADD online ada
-> (integer) 1
SMEMBERS online
SREM online ada            # on clean disconnect""",
                },
            ],
            "cost": (
                "TTL によるプレゼンスは<b>最終的に</b>正しくなるものです。クラッシュ"
                "したクライアントは、最長で TTL のあいだオンラインに見えます——30 秒"
                "という値は、どこまでの古さに耐えられるかに合わせて決めてください。"
                "また <code>SMEMBERS</code> はセット全体を 1 回の返答で返します。"
                "数百万人の名簿なら、代わりに <code>SSCAN</code> でページをめくって"
                "ください。"
            ),
        },
        {
            "t": "recipe",
            "h2": "同じことを、2 つのブラウザタブの間で",
            "goal": "同一オリジンの 2 つのタブ。片方で publish すれば、もう片方が描画します。サーバーも、WebSocket も、接続の状態管理もありません。",
            "cost_t": "コストと制約",
            "items": [
                {
                    "do": "それぞれのタブでエンジンを開く",
                    "code": """import { open } from "@goliapkg/kevy";

const db = await open({ persist: { name: "app" } });""",
                },
                {
                    "do": "タブ A が購読する",
                    "code": """db.subscribe("room:42", (payload, channel) => {
  render(JSON.parse(new TextDecoder().decode(payload)));
});""",
                },
                {
                    "do": "タブ B が publish する——タブ A が描画する",
                    "code": """db.publish("room:42", JSON.stringify({ user: "ada", text: "hello" }));""",
                },
            ],
            "cost": (
                "橋渡しは <code>BroadcastChannel</code> なので、届く範囲は<b>同じ"
                "端末の、同一オリジンのタブ</b>です——絞り込みはエンジンの中で行われ"
                "ますが、端末をまたぐのはサーバーの仕事です。いますぐ試せます。"
                "<a href=\"~/play/\">playground</a> を 2 つのタブで開いて、どちら"
                "からでも publish してみてください。"
            ),
        },
        {
            "t": "cards",
            "h2": "次に",
            "intro": "",
            "items": [
                {"kicker": "ガイド", "title": "pub/sub", "body": "チャネル、パターン、そして遅れた購読者に何が起きるのか。", "go": "読む", "href": "docs/pubsub/"},
                {"kicker": "試す", "title": "2 つのタブ、サーバーなし", "body": "playground を 2 つのタブで開いて、どちらからでも publish してみてください。", "go": "Playground", "href": "#try"},
            ],
        },
    ],
}

# ── /use/ai/ ────────────────────────────────────────────────────────────────

PAGES["use/ai"] = {
    "title": "AI アプリケーションのためのストレージ — kevy",
    "desc": "ベクトル検索、全文検索、変更フィードを、すでにデータを持っているストアの中で。kevy が AI アプリケーションに与えるものと、与えないもの。",
    "foot": "埋め込みモデルは含みません。それは意図した選択です",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "AI アプリケーション",
            "h1": "データと、その見つけ方を<br>ひとつのストアに",
            "lede": (
                "RAG やエージェントの記憶は、たいてい 3 つのシステムを意味します。"
                "キャッシュ、ベクトルデータベース、検索インデックス——同じ事実が "
                "3 つにあり、互いにずれていきます。<b>kevy は、ベクトル KNN、BM25 "
                "全文検索、変更フィードをエンジンに持っています</b>。すでに書いた"
                "キーの上で、そのまま動きます。"
            ),
        },
        {
            "t": "prose",
            "h2": "なぜ向いているのか",
            "body": [
                "RAG のスタックで高くつくのは、検索ではありません。真実の 3 つの"
                "コピーを、歩調を合わせて保つことです。文書を書いたら、それを埋め込み、"
                "索引に入れ、キャッシュを無効化することを、忘れずにやらなければ"
                "なりません。そのどれもが、忘れうる場所です。",
                "<b>kevy では、インデックスはパイプラインではなく宣言です。</b>"
                "どのキーの、どのフィールドかをエンジンに伝えれば、書き込みパスが"
                "インデックスを最新に保ちます。あとから実行するものはなく、遅れて"
                "いくものもありません。",
                "<b>kevy がやらないのは、埋め込みを作ることです。</b>エンジンに"
                "モデルはなく、今後も持ちません。推論はストレージエンジンの仕事では"
                "ありませんし、そうしてしまえば、ベクトルの形式が私たちのリリース"
                "周期に縛られます。ベクトルはあなたが持ち込み、kevy がそれを保存し、"
                "索引を張り、検索します。",
            ],
        },
        {
            "t": "recipe",
            "h2": "キーを、意味で検索する",
            "goal": "すでに書いているキーのフィールドに対する KNN。一度宣言すれば、あとは書き込みパスが最新に保ちます。同期するものはありません。",
            "cost_t": "コストと制約",
            "items": [
                {
                    "do": "インデックスを一度だけ宣言する",
                    "note": "エンジンが既存のキーを埋め戻します。その間の問い合わせには INDEXBUILDING を返します。",
                    "code": """IDX.CREATE idx:sem ON PREFIX doc: FIELD vec TYPE vector KIND ann  DIM 768 DISTANCE cosine M 16 EF 200
-> OK""",
                },
                {
                    "do": "文書は、いままでどおり書く",
                    "code": """HSET doc:4410 title "Ada on pipelining" vec "<768 f32, little-endian>\"""",
                },
                {
                    "do": "近い順に 10 件",
                    "code": """IDX.QUERY idx:sem KNN "<query vector>" LIMIT 10
-> 1) doc:4410
   2) doc:9982""",
                },
            ],
            "cost": (
                "<b>インデックスは HNSW で、近似です</b>。再現率は保証ではなく、"
                "調整のためのパラメータ(<code>EF</code>)です。最初の構築は、合致"
                "するキーに対して O(N) です——行き当たるのではなく、計画してください。"
                "そして<b>埋め込みモデルはありません</b>。ベクトルは、あなたが持ち込み"
                "ます。調整のつまみは<a href=\"~/docs/vector-search/\">ベクトルの"
                "ガイド</a>にあります。"
            ),
        },
        {
            "t": "recipe",
            "h2": "全文検索、そして両方のランキングを融合する",
            "goal": "同じキーに対する BM25。そしてテキストのランキングとベクトルのランキングを、1 つのコマンドで融合するハイブリッドクエリ。",
            "cost_t": "コストと制約",
            "items": [
                {
                    "do": "同じキーに、テキストのインデックスを張る",
                    "code": """IDX.CREATE idx:ft ON PREFIX doc: FIELD title TYPE str KIND text
-> OK""",
                },
                {
                    "do": "マッチを、BM25 の順位つきで",
                    "code": """IDX.QUERY idx:ft MATCH "pipelining"
-> 1) 1) "doc:1"
      2) "0.2877"          # the BM25 score""",
                },
                {
                    "do": "ハイブリッド——両方のランキングを融合する(RRF)",
                    "code": """IDX.QUERY HYBRID idx:ft MATCH "pipelining" idx:sem KNN "<vector>"  LIMIT 20 RRFK 60""",
                },
            ],
            "cost": (
                "インデックスの代金は、合致するキーへの<b>書き込みのたびに</b>支払い"
                "ます——読み取り主体の検索には正しい取引で、毎秒何千回も書き直すキー"
                "には間違った取引です。トークン化(CJK を含みます)と、BM25 がどこで"
                "止まるのかは、<a href=\"~/docs/text-search/\">テキストのガイド</a>に"
                "あります。"
            ),
        },
        {
            "t": "recipe",
            "h2": "エージェントの記憶を、遅れさせない",
            "goal": "別のプロセスから、すべての書き込みを追いかけます——スケジュールではなく変更のたびに埋め込み、止めたところから再開します。",
            "cost_t": "コストと制約",
            "items": [
                {
                    "do": "フィードを有効にする",
                    "code": """# kevy.toml
[feed]
enabled = true""",
                },
                {
                    "do": "自分のカーソルを確かめる",
                    "code": """FEED.SHARDS                 -> (integer) 16
FEED.TAIL 0                 -> 1) (integer) 1     # generation
                               2) (integer) 1     # offset""",
                },
                {
                    "do": "読み、処理し、再開する",
                    "code": """FEED.READ 0 1 0 COUNT 2     -> the writes themselves, replayable""",
                },
            ],
            "cost": (
                "フィードはシャード単位です。<code>FEED.SHARDS</code> が、あなたの"
                "持つカーソルの数を教え、コンシューマはシャードごとにオフセットを "
                "1 つ追跡します。既定ではオフになっており、<code>[feed]</code> を"
                "オンにすることが、書き込みパスの帳簿づけの代金です。再起動をまたぐ"
                "再開は、<a href=\"~/docs/cdc/\">変更フィードのガイド</a>が扱って"
                "います。"
            ),
        },
        {
            "t": "callout",
            "kind": "note",
            "title": "これを読んでいるのがエージェントなら",
            "body": (
                "<a href=\"~/llms-full.txt\">llms-full.txt</a> は、1 回の取得で"
                "済みます。全コマンドと、その本当のコストと Redis との本当の差異、"
                "そしてすべてのガイドの全文が入っています。エンジン自身のコマンド表から"
                "生成しているので、サーバーの実際の挙動とずれることはありません。"
            ),
        },
        {
            "t": "cards",
            "h2": "次に",
            "intro": "",
            "items": [
                {"kicker": "ガイド", "title": "ベクトル検索", "body": "HNSW、調整のつまみ、そしてここでの「近似」が実際に意味するもの。", "go": "読む", "href": "docs/vector-search/"},
                {"kicker": "ガイド", "title": "全文検索", "body": "BM25、CJK を含むトークン化、そして、どこまでで止まるのか。", "go": "読む", "href": "docs/text-search/"},
                {"kicker": "ガイド", "title": "変更フィード", "body": "別のプロセスから、すべての書き込みを追いかける。再開可能なオフセットつき。", "go": "読む", "href": "docs/cdc/"},
            ],
        },
    ],
}

# ── /use/app-store/ ─────────────────────────────────────────────────────────

PAGES["use/app-store"] = {
    "title": "データベースなしで読みを捌く — kevy",
    "desc": "kevy のセカンダリインデックスとマテリアライズドビュー。絞り込んだ一覧や集計を、クエリにせず参照のまま保つ方法。",
    "foot": "ほとんどのアプリケーションが、ORM に実際に求めている部分",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "主ストア",
            "h1": "読み取りを、参照のまま保つ",
            "lede": (
                "「この顧客の、まだ open な注文をすべて」。「このカートに何点"
                "入っているか」。これらはアプリケーションが毎秒何千回も行う読み取り"
                "であり、リレーショナルデータベースでは、そのひとつひとつが、裏に"
                "プランナを抱えたクエリになります。<b>kevy なら、答えを用意して"
                "おけます。</b>"
            ),
        },
        {
            "t": "prose",
            "h2": "なぜ向いているのか",
            "body": [
                "キーバリューストアをアプリケーションのデータに使うことは、たいてい"
                "ひとつの反論で退けられます。<i>キー以外のもので引きたいのだが</i>、"
                "と。その反論は正しく、セカンダリインデックスは、まさにそのために"
                "あります。",
                "<b>インデックスは、構築するものではなく宣言するものです。</b>キーの"
                "パターンとフィールドを指定すれば、書き込みパスが、それを最新に保ち"
                "ます。絞り込んだ一覧は、ふたたび参照になります。プランナもスキャンも"
                "クエリもありません。",
                "<b>ビューはさらに進んで</b>、書き込み時に集計を最新に保ちます。件数や"
                "合計は、計算されるのではなく読まれます。ほとんどのアプリケーションが "
                "ORM に実際に求めているのはこれであり、データベースが忙しい理由も、"
                "これです。",
                "<b>そして、テーブル全体を一度に宣言できます。</b>"
                "<code>TABLE.DECLARE</code> は型付きカラム、セカンダリインデックス、"
                "複合 <code>ORDER BY</code> パスを受け取り、宣言の時点で名前つき"
                "インデックスへコンパイルします——<code>kevy-sql</code> は、手元の "
                "PG/MySQL スキーマファイルから同じことをします。エンジンは相変わらず"
                "何もプランせず、スキーマを課しません。join とランタイム SQL は、"
                "名前つきで拒否されたままです。",
            ],
        },
        {
            "t": "recipe",
            "h2": "キーではなく、フィールドで引く",
            "goal": "「顧客 881 の注文をすべて」が、参照のままです。引きたいフィールドごとにインデックスを宣言し、普通に書き、値で読みます。",
            "cost_t": "コストと制約",
            "items": [
                {
                    "do": "データは、いままでどおり書く",
                    "code": """HSET order:1001 customer 881 status open  total 4400
HSET order:1002 customer 881 status paid  total 8400
HSET order:1003 customer 902 status open  total 1200""",
                },
                {
                    "do": "引きたいフィールドごとに、インデックスを 1 つ",
                    "code": """IDX.CREATE idx:cust   ON PREFIX order: FIELD customer TYPE i64 KIND range
IDX.CREATE idx:status ON PREFIX order: FIELD status   TYPE str KIND range""",
                },
                {
                    "do": "クエリになるはずだった読み取り",
                    "code": """IDX.QUERY idx:cust EQ 881
-> 1) "0"                       # cursor
   2) 1) "order:1001"  2) "881"
      3) "order:1002"  4) "881\"""",
                },
                {
                    "do": "条件を 2 つ、同時に",
                    "code": """IDX.QUERY COMPOSE AND idx:cust EQ 881 idx:status EQ open
-> 1) "0"
   2) 1) 1) "order:1001\"""",
                },
            ],
            "cost": (
                "<b>インデックスの代金は、読み取りではなく書き込みのたびに支払い"
                "ます</b>——読み取り主体の配信には正しく、書き込み主体のログには"
                "間違った取引です。<b>結合はありません</b>し、今後も持ちません。"
                "インデックスが答えるのは「どのキーがこれらのフィールドに合致する"
                "か」であって、「この 2 つのコレクションを結合せよ」ではありません。"
                "読み取りに本当に結合が要るなら、Postgres に置いておいてください——"
                "どれがそれに当たるのかは、<a href=\"~/docs/rds-workloads/\">RDS "
                "ワークロードのページ</a>に書いてあります。"
            ),
        },
        {
            "t": "recipe",
            "h2": "更新され続ける答えを、用意しておく",
            "goal": "絞り込まれ、順序のついた一覧を、書き込みパスが維持します——古くなることが決してないので、読み取りが計算し直すこともありません。",
            "cost_t": "コストと制約",
            "items": [
                {
                    "do": "同じインデックスの上に、ビューを宣言する",
                    "note": "括弧は、それぞれ独立した引数です。",
                    "code": """VIEW.CREATE v:open881 QUERY ( AND idx:cust EQ 881 idx:status EQ open )  ORDER BY idx:cust
-> OK""",
                },
                {
                    "do": "読む——ここでは何も計算されません",
                    "code": """VIEW.QUERY  v:open881
-> 1) "0"
   2) 1) "order:1001"  2) "881\"""",
                },
            ],
            "cost": (
                "ビューは<b>書き込みパスの、終わらない仕事</b>です。合致するキーへの"
                "書き込みは、今日それを読む人がいるかどうかに関係なく、毎回ビューを"
                "更新します。アプリケーションが実際に捌いている読み取りのために"
                "ビューを宣言し、捌かなくなったら落としてください。ビューが組み"
                "合わせるインデックスは、先に存在している必要があります。"
            ),
        },
        {
            "t": "recipe",
            "h2": "テーブル全体を、一度で宣言する",
            "goal": "リレーショナルなテーブルの読み取りパス——インデックスされた WHERE、残りのフィルタ、ORDER BY、ページング、COUNT——を、宣言 1 つで名前つきインデックスへコンパイルします。手元のスキーマファイルからでも。",
            "cost_t": "コストと制約",
            "items": [
                {
                    "do": "カラムも、インデックスも、ソートパスも、宣言 1 つで",
                    "note": "行はプレフィックス配下の普通のハッシュのままです——欠けたカラムは NULL。kevy-cli sql compile schema.sql が、CREATE TABLE / CREATE INDEX からこの行を出力します。",
                    "code": """TABLE.DECLARE orders PREFIX order: PK id COLUMN id str COLUMN customer i64 COLUMN status str COLUMN total f64 INDEX status range VALUES total customer ORDERPATH by_customer ON customer THEN total DESC
-> OK""",
                },
                {
                    "do": "保存されたカラムでフィルタと集計——行は 1 つも読みません",
                    "code": """IDX.QUERY orders.status EQ open FILTER total RANGE 2000 inf LIMIT 20
-> 1) "0"
   2) 1) "order:1001"  2) "open"

IDX.COUNT orders.status EQ open
-> (integer) 2""",
                },
                {
                    "do": "ORDER BY customer, total DESC の歩き方",
                    "note": "リレーショナルの複合インデックスと同じやり方で、複合インデックス 1 本が答えます——顧客ごとの注文を、大きい順に、再ソートなしで。",
                    "code": """IDX.QUERY orders.by_customer WHERE customer EQ 881 LIMIT 20 FIELDS status total""",
                },
            ],
            "cost": (
                "<b>ランタイム SQL も join もありません。</b>サーバーは "
                "<code>SELECT</code> を未知のコマンドとして拒否します。"
                "<code>kevy-cli sql compile</code> はビルド時に PG/MySQL のスキーマ"
                "ファイルを上の宣言に変え、JOIN、サブクエリ、GROUP BY を名前つきで"
                "拒否して、それぞれを置き換えるレシピを指し示します。一意性は強制では"
                "なく検証で、制約はエンジンのチェックではなくレシピです。"
                "<a href=\"~/docs/tiering/\">ティアリング</a>をオンにすれば、"
                "index-only クエリは全行コールドでも RAM だけで答えます——コールドな"
                "行を読むのは最後の <code>FIELDS</code> ページだけ、1 行 1 回です。"
            ),
        },
        {
            "t": "cards",
            "h2": "次に",
            "intro": "",
            "items": [
                {"kicker": "ガイド", "title": "テーブル", "body": "型付きカラムとインデックスを一度宣言すれば、テーブルのように引ける。", "go": "読む", "href": "docs/tables/"},
                {"kicker": "ガイド", "title": "kevy での設計", "body": "テーブルで考えることに慣れた頭で、キーで考える方法。", "go": "読む", "href": "docs/designing-on-kevy/"},
                {"kicker": "ガイド", "title": "セカンダリインデックス", "body": "どう構築され、何を要し、クエリプランをどう説明するのか。", "go": "読む", "href": "docs/indexes/"},
                {"kicker": "リファレンス", "title": "RDS ワークロード", "body": "リレーショナルな全パターンと、ここでやる場合の正直なコスト。", "go": "読む", "href": "docs/rds-workloads/"},
            ],
        },
    ],
}

# ── /use/embedded/ ──────────────────────────────────────────────────────────

PAGES["use/embedded"] = {
    "title": "kevy を組み込む — kevy",
    "desc": "ストアを、プログラムの中へ。デスクトップアプリ、ブラウザのタブ、エッジワーカー、OS のないマイコン。",
    "foot": "ひとつのエンジン、4 つの置き場所、サーバーなし",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "中に組み込む",
            "h1": "ストアを、<br>そのものの中へ",
            "lede": (
                "サーバーもソケットもネットワークもありません。エンジンは、呼び出せる "
                "struct であり、481 KB の WebAssembly モジュールであり、OS のない"
                "チップの上の no_std ライブラリです——<b>そして 3 つとも、同じ"
                "コマンドを持つ、同じエンジンです。</b>"
            ),
        },
        {
            "t": "prose",
            "h2": "なぜ向いているのか",
            "body": [
                "オフラインで動く必要のあるアプリケーションは、どれも結局ストレージ層"
                "を書くことになります。デスクトップアプリは SQLite と、誰も望まなかった"
                "スキーマを手に入れます。Web アプリは localStorage を使い、5 MB の"
                "上限と、同期でしかも文字列しか扱えないという事実を知り、次に "
                "IndexedDB とその上の抽象を手に入れます。デバイスは、フラッシュ上に"
                "手書きのリングバッファを手に入れます。",
                "<b>3 つとも同じ問題であり、同じ解になりえます。</b>kevy はプロセス"
                "境界なしで組み込め、本物の TTL と pub/sub を持ったままブラウザに"
                "届き、固定 arena とアロケータなしで Cortex-M でも起動します。"
                "最後のひとつは、CI が push のたびに証明しています。",
            ],
        },
        {
            "t": "recipe",
            "h2": "Rust のプログラムの中で",
            "goal": "ストアは、呼び出せる struct です——ソケットも、シリアライズも、2 つ目のプロセスもありません。永続化され、開くときにログを再生します。",
            "cost_t": "コストと制約",
            "items": [
                {
                    "do": "追加する",
                    "code": """# Cargo.toml
kevy-embedded = "4.0\"""",
                },
                {
                    "do": "開いて、書いて、読む",
                    "code": """let db = Db::open("data/")?;
db.set(b"session:7f3a", b"{\\"user\\":\\"ada\\"}", Some(Duration::from_secs(3600)))?;
assert_eq!(db.get(b"session:7f3a")?.is_some(), true);""",
                },
                {
                    "do": "あとで redis-cli が要るなら、リスナーを開く",
                    "note": "他のプロセスが、同じストアに RESP で届くようになります。上のコードは、何ひとつ変わりません。",
                    "code": """db.listen("127.0.0.1:6379")?;""",
                },
            ],
            "cost": (
                "<b>組み込みのストアは、共有されません。</b>データディレクトリを所有"
                "するのは 1 つのプロセスです。2 つ目のプロセスがデータを必要とする"
                "なら、そのためにあるのが上のリスナー——あるいは"
                "<a href=\"~/docs/embedded-listener/\">フルのサーバー</a>——です。"
                "ストアはプロセス内でも RAM 予算を取れます"
                "(<code>with_tier_budget</code>)。コールドな値はディスクへ退避し、"
                "あなたのプロセスの中で戻ります——正直な注記として、プロセス内の"
                "コールド読み取りは、その読み取りの間ストアのロックを保持します。"
                "退避できる値の最大が既定で 256 KiB に制限されているのは、そのためです。"
                "詳細は<a href=\"~/docs/tiering/\">ティアリングのガイド</a>にあります。"
            ),
        },
        {
            "t": "recipe",
            "h2": "ブラウザのタブで",
            "goal": "gzip 後 481 KB。ブラウザ自身のファイルシステムに永続化され、リロードに耐え、タブをまたいで pub/sub を話します。",
            "cost_t": "コストと制約",
            "items": [
                {
                    "do": "永続化つきで開く",
                    "code": """import { open } from "@goliapkg/kevy";

const db = await open({ persist: { name: "app" } });""",
                },
                {
                    "do": "本物の TTL で書き、リロードのあとに読む",
                    "code": """db.set("cart:u881", JSON.stringify(items), { ttlMs: 86_400_000 });
db.get("cart:u881");        // still there after a reload
db.pttl("cart:u881");       // the engine expires it, not your code""",
                },
                {
                    "do": "他のタブの声を聞く",
                    "code": """db.subscribe("sync", (payload) => merge(payload));""",
                },
            ],
            "cost": (
                "<b>小さな同期の読み取りなら、localStorage のほうが速いです</b>——"
                "あれはページ自身のアドレス空間にあるマップであり、OPFS の上に作られた"
                "ものが、そこで勝つことはありません。kevy が勝つのは、そもそも "
                "localStorage を選ぶべきでない理由のほうです。本物の TTL、5 MB の"
                "上限がないこと、値が文字列ではなくバイト列であること、そして書き込み"
                "がメインスレッドを止めないこと。"
            ),
        },
        {
            "t": "recipe",
            "h2": "マイコンの上で",
            "goal": "no_std、アロケータなし、OS なし。ストアは、大きさを自分で決める固定の arena に住みます。CI が push のたびに、これを起動させています。",
            "cost_t": "コストと制約",
            "items": [
                {
                    "do": "そぎ落とす",
                    "code": """# Cargo.toml
kevy-store = { version = "4.0", default-features = false }""",
                },
                {
                    "do": "メモリを渡して、使う",
                    "code": """let mut arena = [0u8; 64 * 1024];
let mut store = Store::new_in(&mut arena);
store.set(b"temp", b"21.4")?;""",
                },
            ],
            "cost": (
                "<b>arena は固定です。</b>実行中に広げることはできません——"
                "「アロケータなし」とは、そういうことです。大きさを決めるのは"
                "エンジンではなく、あなたの設計です。機能の段階と、それぞれが"
                "何バイト要るのかは、<a href=\"~/docs/iot/\">IoT のガイド</a>に"
                "あります。"
            ),
        },
        {
            "t": "cards",
            "h2": "次に",
            "intro": "",
            "items": [
                {"kicker": "ガイド", "title": "WebAssembly の kevy", "body": "ブラウザ向けビルド、OPFS への永続化、そしてサイズの予算。", "go": "読む", "href": "docs/wasm/"},
                {"kicker": "ガイド", "title": "組み込みリスナー", "body": "エンジンを組み込んだまま、ソケットで RESP を話す。", "go": "読む", "href": "docs/embedded-listener/"},
                {"kicker": "ガイド", "title": "IoT とベアメタル", "body": "no_std、arena、そして機能の段階。", "go": "読む", "href": "docs/iot/"},
            ],
        },
    ],
}

# ── /benchmarks/ ────────────────────────────────────────────────────────────
# Evidence, not a story. Whatever we learned getting the measurement right is in
# bench/ — a reader here wants to know whether the numbers can be trusted and
# where they do not hold, not how we arrived at them.

PAGES["benchmarks"] = {
    "title": "ベンチマーク — kevy",
    "desc": "1 台のマシンでの kevy 4.0 と Redis 8、valkey 9.1、Dragonfly の比較——kevy がかろうじて上回っているだけのコマンドも含めて。",
    "foot": "リポジトリの bench/ から再現できます",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "ベンチマーク",
            "h1": "どれだけ速いか、そして、どこで速くないか",
            "lede": (
                "1 台のマシン、16 コア、ループバック。すべての数字は、リポジトリの "
                "<code>bench/</code> から再現できます。<b>何かを決める前に、最後の "
                "2 行を読んでください</b>——速さが乗り換える理由にならないのは、"
                "その 2 行です。"
            ),
        },
        {
            "t": "table",
            "h2": "4 つのエンジン、1 台のマシン",
            "intro": (
                "コネクション 50、小さな値。5 回実行した中央値です。ベンチマーク"
                "クライアントが報告する速度ではなく、各サーバー自身のコマンド"
                "カウンタを、3 秒間の定常状態で数えました。"
            ),
            "head": ["", "kevy 6.2.2", "Redis 8", "valkey 9.1", "Dragonfly", "Redis 8 比"],
            "rows": [
                ["GET", "7,395,730", "5,599,436", "3,086,168", "2,845,294", "*1.32×"],
                ["SET", "6,305,322", "2,551,278", "1,694,840", "1,924,695", "*2.47×"],
                ["INCR", "6,294,330", "3,326,620", "2,221,391", "2,031,132", "*1.89×"],
                ["SADD", "4,874,874", "3,788,956", "2,192,994", "1,800,121", "*1.29×"],
                ["HSET", "4,511,460", "3,043,259", "1,863,456", "1,768,012", "*1.48×"],
                ["LPUSH", "3,088,809", "2,788,277", "1,873,136", "1,461,737", "!1.11×"],
                ["ZADD", "3,508,110", "2,816,824", "1,794,137", "1,714,133", "*1.25×"],
            ],
            "note": (
                "<b>LPUSH は Redis 8 より 12%、ZADD は 10% 速いだけです。</b>この差"
                "では、勝敗を決めるのはエンジンではなく、値のサイズとキーの分布です。"
                "リストやソート済みセットがホットパスなら、自分のワークロードで"
                "測ってください。速さを理由に乗り換えてはいけません。この 2 行の色は、"
                "そのために付けてあります。"
            ),
        },
        {
            "t": "prose",
            "h2": "この数字が、教えてくれないこと",
            "body": [
                "<b>ループバックです。</b>ここにネットワークはありませんが、実際の"
                "運用で待たされる相手は、たいていネットワークです。GET が 2.6 倍速い"
                "エンジンでも、レイテンシの大半が回線なら、p99 が 2.6 倍良くなること"
                "はありません。",
                "<b>値が小さいです。</b>1 値あたり 64 KB になると、全体がカーネルの "
                "TCP パスに律速され、差は 1 桁台まで縮まります。大きなブロブを保存"
                "するなら、この数字は、あなたの話ではありません。",
                "<b>1 台のマシンです。</b>kevy にクラスタモードはありません。1 台では"
                "足りないことが問題なら、このページのどの数字も助けになりません。",
            ],
        },
        {
            "t": "table",
            "h2": "ブラウザ向けビルド",
            "intro": "タブに実際に配るもの。",
            "head": ["", "サイズ", ""],
            "rows": [
                ["kevy.wasm", "1442 KB", "エンジン本体、非圧縮"],
                ["gzip 後", "481 KB", "回線を流れる量"],
                ["コールドスタート", "&lt; 20 ms", "コンパイルとインスタンス化、キャッシュが温まった状態"],
            ],
            "note": (
                "<b>小さな同期の読み取りでは、localStorage が kevy に勝ちます</b>。"
                "今後もそうです——あれはページ自身のアドレス空間にあるマップです。"
                "kevy が勝つのは、そもそも localStorage を選ぶべきでない理由のほう"
                "です。本物の TTL、5 MB の上限がないこと、値が文字列ではなくバイト列"
                "であること、書き込みがメインスレッドを止めないこと。"
            ),
        },
        {
            "t": "code",
            "h2": "再現する",
            "caption": "スクリプトは 2 つ。このページの内容は、すべてそこから出てきます。",
            "text": "git clone https://github.com/goliajp/kevy && cd kevy\n\n# four-way: kevy, Redis 8, valkey, Dragonfly\nbash bench/arena.sh\n\n# the regression gate CI runs on every push\nbash bench/perfgate.sh",
        },
    ],
}

PAGES["capacity"] = {
    "title": "容量計算機 — kevy",
    "desc": "固定 RAM 予算 + 階層化のもとで、一つの kevy プロセスがどれだけのデータを提供できるか：実測式のインタラクティブ版。",
    "foot": "式は実測 RSS に対して ±20% でゲートされている",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "容量",
            "h1": "どれだけのデータが収まるか？",
            "lede": (
                "<a href=\"~/docs/tiering/\">階層化</a>により、kevy は RAM より大きな"
                "キー空間を提供します：ホットな値は常駐し、コールドな値はディスクへ、"
                "各キーは常駐スタブを保ちます。天井は一つの実測値——エントリあたり約 96 B の"
                "フロア——で決まり、このページはその式のインタラクティブ版です。"
                "<b>答えはあなたの値サイズ次第</b>；正直な容量の数字はステッカーには収まりません。"
            ),
        },
        {
            "t": "calc",
            "h2": "あなたの数字",
            "intro": (
                "max data:RAM ≈ 値サイズ / (96 B + キーのヒープ分)。22 B 以下のキーは"
                "インラインで追加コストなし；それより長いキーは自身のバイト数を加算。"
                "64 B 未満の値は決して階層化されません——スタブが値と同じ大きさだからです。"
            ),
            "fields": {
                "value": "典型的な値サイズ（バイト）",
                "key": "典型的なキーサイズ（バイト）",
                "budget": "RAM 予算（GB）",
                "ratio": "data:RAM の上限",
                "served": "その予算で提供できるデータ量",
                "below": "64 B 未満の値は階層化されません——スタブが値と同じ大きさで、比率は 1× のまま。容量は RAM そのものです。",
                "note": "これはモデルであって約束ではありません：実際の比率はゲートの ±20% 帯に収まり、下の実測行はモデルよりわずかに低くなります。",
            },
        },
        {
            "t": "table",
            "h2": "実測：同じ予算、同じキー",
            "intro": "値サイズだけを変えた結果。完全なデータはリポジトリの capacity findings に。",
            "head": ["値サイズ", "モデルの予測", "実測 data:RAM"],
            "rows": [
                ["256 B", "2.67×", "2.65×"],
                ["1 KiB", "10.7×", "10.43×"],
                ["4 KiB", "42.7×", "39.2× —— フルスケール：2 GB 予算で 80 GB を提供"],
            ],
        },
        {
            "t": "callout",
            "kind": "info",
            "title": "その 96 B はどこへ行くのか",
            "body": (
                "キー空間のエントリ自体——インラインのキーセルとエントリヘッダ——であり、"
                "階層化の有無にかかわらずすべてのキーが払います；コールドスタブはヒープを"
                "持ちません。フロアとスピル閾値は CI でゲートされているので"
                "（<code>memgate</code>、±20%）、このページがエンジンから静かに"
                "漂流することはありません。詳細：<a href=\"~/docs/tiering/\">階層化</a>。"
            ),
        },
    ],
}

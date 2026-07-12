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
    "desc": "AI システムのために作られた、Redis 互換のデータレイヤです。プロトコルは同じまま、より速く。そしてベクトル検索、全文検索、インデックス、ビュー、変更フィードが同じエンジンに入っています——サーバーでも、バイナリの中でも、ブラウザのタブでも、デバイスの上でも。",
    "foot": "GOLIA",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "kevy 4.0",
            "h1": "AI システムのための<br>データレイヤ。",
            "lede": (
                "Redis 互換なので、そのまま差し替えられます。<b>どの操作でも、"
                "より高速です。</b>そして AI システムが実際に必要とするもの——"
                "ベクトル検索、全文検索、セカンダリインデックス、マテリアライズド"
                "ビュー、変更フィード——を、<b>同じエンジンの中で、同じキーの上で"
                "</b>提供します。"
            ),
            "ctas": [
                {"label": "なぜ Redis を置き換えられるのか", "href": "#swap"},
                {"label": "他に何ができるのか", "href": "#more"},
                {"label": "ブラウザで動かす", "href": "play/"},
            ],
            "aside": '<pre class="hero-code"><code>$ cargo install kevy\n$ kevy --port 6379\n\n$ redis-cli -p 6379\n&gt; SET session:7f3a \'{"user":"ada"}\' EX 3600\nOK\n&gt; IDX.QUERY idx:sem KNN "&lt;vector&gt;" LIMIT 10\n1) "doc:4410"\n2) "doc:9982"</code></pre>',
        },
        {
            "t": "bars",
            "id": "swap",
            "tone": "deep",
            "eyebrow": "なぜ Redis を置き換えられるのか",
            "h2": "プロトコルは同じ。スループットは上。",
            "intro": (
                "クライアントは変わりません——RESP2 と RESP3、184 個のコマンド、"
                "いま使っているライブラリのまま。1 台のマシン、16 コア、ループバック、"
                "小さな値、5 回実行した中央値です。"
            ),
            # name, kevy, redis 8, ratio, thin?
            "rows": [['GET', 7800299, 5597865, '1.39×', False], ['SET', 6918058, 2573396, '2.69×', False], ['INCR', 6133940, 3459395, '1.77×', False], ['SADD', 5600597, 3690483, '1.52×', False], ['HSET', 4287217, 3021325, '1.42×', False], ['LPUSH', 3213470, 2862374, '1.12×', True], ['ZADD', 3053101, 2773929, '1.10×', True]],
            "us": "kevy 4.0",
            "them": "Redis 8",
            "thin": "差は 15% 未満——勝敗を決めるのはエンジンではなく、あなたのワークロードです",
            "note": (
                "<b>LPUSH と ZADD は、12% と 10% しか上回っていません。</b>この差では、"
                "勝敗を決めるのは値のサイズとキーの分布です。リストやソート済みセットが"
                "ホットパスなら、速さは乗り換える理由になりません。"
                "<a href=\"benchmarks/\">valkey や Dragonfly も含めた、完全な表は"
                "こちら。</a>"
            ),
        },
        {
            "t": "cards",
            "id": "more",
            "eyebrow": "他に何ができるのか",
            "h2": "AI システムに必要なものが、<br>データをすでに持っているエンジンの中に。",
            "intro": "モジュールではありません。サイドカーでもありません。元データからずれていく、真実の 2 つ目のコピーでもありません。",
            "items": [
                {
                    "kicker": "ベクトル",
                    "title": "キースペースに対する KNN",
                    "body": "HNSW インデックスを一度宣言すれば、あとは書き込みの側が最新に保ちます。埋め込みはあなたが持ち込み、kevy がそれを保存し、索引を張り、検索します。",
                    "go": "手順",
                    "href": "use/ai/",
                },
                {
                    "kicker": "全文検索",
                    "title": "BM25、そしてハイブリッドな順位付け",
                    "body": "同じキーに対する全文検索と、テキストの順位付けとベクトルの順位付けを融合するハイブリッドクエリ。",
                    "go": "手順",
                    "href": "use/ai/",
                },
                {
                    "kicker": "インデックス",
                    "title": "任意のフィールドで引く",
                    "body": "セカンダリインデックスが、絞り込んだ読み取りを、ふたたび参照に戻します。クエリプランナも、スキャンもありません。",
                    "go": "手順",
                    "href": "use/app-store/",
                },
                {
                    "kicker": "ビュー",
                    "title": "答えを、用意しておく",
                    "body": "マテリアライズドビューが書き込みの側で集計を最新に保つので、読み取りが計算し直すことはありません。",
                    "go": "手順",
                    "href": "use/app-store/",
                },
                {
                    "kicker": "変更フィード",
                    "title": "すべての書き込みを追う",
                    "body": "別のプロセスが——あるいはエージェントが——追いかけられる、再開可能なフィード。ポーリングは要りません。",
                    "go": "手順",
                    "href": "use/ai/",
                },
                {
                    "kicker": "どこでも",
                    "title": "サーバー、バイナリ、ブラウザ、デバイス",
                    "body": "同じエンジン、同じコマンドのままです。16 コアのサーバー、バイナリの中、ブラウザのタブで 151 KB、あるいは OS のないチップの上。",
                    "go": "手順",
                    "href": "use/embedded/",
                },
            ],
        },
        {
            "t": "prose",
            "tone": "blue",
            "h2": "なぜ AI システムには、別のデータレイヤが要るのか",
            "body": [
                "エージェントは、文書を書き、それを埋め込み、索引に入れ、キャッシュし、"
                "変更があったことを別の何かに伝えます。いまはそれが 4 つのシステムです"
                "——キャッシュ、ベクトルデータベース、検索インデックス、キュー。同じ"
                "事実を抱えたまま、互いにずれていきます。そのどれもが、手順を忘れうる"
                "場所です。",
                "<b>kevy は、それをひとつにまとめます。</b>インデックスを宣言し、キーは"
                "いままでどおりに書くだけです。ベクトルインデックス、テキスト"
                "インデックス、セカンダリインデックス、そしてビューを、エンジンが"
                "書き込みの側で最新に保ちます。そして変更フィードが、聞いている相手に"
                "それを伝えます。",
                "そして kevy は、エージェントが動く場所で動きます。あなたのサービスの"
                "中で、ソケットを持たないバイナリの中で、WebAssembly としてブラウザの"
                "タブの中で、あるいはネットワークの端にあるデバイスの上で。",
            ],
        },
        {
            "t": "cards",
            "tone": "deep",
            "h2": "何を作っていますか",
            "intro": "それぞれのページに、kevy が合うかどうか、何を差し出すことになるか、そして具体的な手順を——貼り付けて実行できるコマンドとともに書いてあります。",
            "items": [
                {"kicker": "AI", "title": "エージェントの記憶と RAG", "body": "ベクトル、全文検索、変更フィード、そしてセッションを勝手に期限切れにしてくれる TTL。", "go": "手順", "href": "use/ai/"},
                {"kicker": "配信", "title": "データベースなしの読み取り", "body": "インデックスとビューによって、読み取りはクエリにならず、参照のままです。", "go": "手順", "href": "use/app-store/"},
                {"kicker": "キャッシュ", "title": "セッションとレート制限", "body": "そもそもデータベースの問題ではなかった行を、データベースから外します。", "go": "手順", "href": "use/cache/"},
                {"kicker": "キュー", "title": "バックグラウンドジョブ", "body": "コンシューマグループつきのストリーム。ジョブは、それを抱えていたワーカーより長く生き残ります。", "go": "手順", "href": "use/queue/"},
                {"kicker": "リアルタイム", "title": "つながっているクライアントに送る", "body": "パターン購読つきの pub/sub。そしてサーバーなしで、ブラウザのタブをまたいで。", "go": "手順", "href": "use/realtime/"},
                {"kicker": "組み込み", "title": "そのものの中に入れる", "body": "デスクトップアプリ、ブラウザ、エッジワーカー、マイコン。サーバーもソケットもありません。", "go": "手順", "href": "use/embedded/"},
            ],
        },
        {
            "t": "steps",
            "h2": "はじめる",
            "intro": "",
            "items": [
                {
                    "title": "サーバーとして",
                    "body": "6379 番で RESP。redis-cli も、クライアントライブラリも、何かが変わったことに気づきません。",
                    "code": 'cargo install kevy\nkevy --port 6379',
                },
                {
                    "title": "Rust のプログラムの中で",
                    "body": "ソケットも、2 つ目のプロセスも、シリアライズもありません。",
                    "code": 'kevy-embedded = "4.0"\n\nlet db = Db::open("data/")?;\ndb.set(b"k", b"v", None)?;',
                },
                {
                    "title": "ブラウザのタブで",
                    "body": "151 KB。ブラウザのファイルシステムに永続化され、リロードにも耐えます。",
                    "code": 'import { open } from "@goliajp/kevy";\n\nconst db = await open({ persist: { name: "app" } });\ndb.set("cart:u1", json, { ttlMs: 3_600_000 });',
                },
            ],
        },
        {
            "t": "callout",
            "kind": "loss",
            "tone": "deep",
            "title": "kevy がやらないこと",
            "body": (
                "<b>クラスタではありません。</b>レプリケーションとフェイルオーバーは"
                "ありますが、マシンをまたぐデータのシャーディングはなく、今後もあり"
                "ません。1 台で足りないなら、kevy は間違った答えです。<b>AUTH も TLS "
                "もありません</b>——プライベートなネットワークで動かすか、それらを"
                "正しく処理するものの後ろに置いてください。そして<b>いくつかの"
                "コマンドは Redis と挙動が違います</b>。<code>SCAN</code> はカーソル"
                "による反復ではなく、<code>ZRANK</code> は O(N)、<code>SPOP</code> は"
                "ランダムではありません。<a href=\"docs/commands/\">違いはすべて、"
                "コマンドごとに書き出してあります</a>。<a href=\"choose/\">そして、"
                "そもそも使うべきでない場合はこちら。</a>"
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
                "184 個のコマンドに応答します。既存のライブラリの接続先を変えるだけで、"
                "コードもそのまま、redis-cli もそのままです。新しく覚える SDK も"
                "プロトコルもありません。",
                "<b>だから本当の問題は、何が得られるのかだけです。</b>得られるものは "
                "3 つあります。そのどれにも価値を感じないなら、Redis に留まって"
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
                    "title": "監査できるバイナリが、ひとつ",
                    "body": "サードパーティ依存はゼロ。crate は 33 個、作者は 1 人、サプライチェーンは半日で読み切れます。規制のあるビルド、vendoring するビルド、顧客のマシンに配るものにとって、これは些細な利点ではありません。",
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
                "コマンドは挙動が違います</b>——<code>SCAN</code> はカーソルによる反復"
                "ではなく(1 回の呼び出しですべてを返し、カーソルは 0 になります)、"
                "<code>ZRANK</code> は O(N)、<code>SPOP</code> と "
                "<code>SRANDMEMBER</code> はランダムではなく、シャードをまたぐ "
                "<code>RENAME</code> は原子的ではありません。どれもバグではなく、"
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
                "<b>そして、問い合わせの一部も移せます。</b>セカンダリインデックスと"
                "マテリアライズドビューがあれば、絞り込んだ一覧や集計は、クエリになる"
                "必要がありません。書き込みの側が、答えを用意しておきます。ほとんどの"
                "アプリケーションが ORM に実際に求めているのは、その部分です。",
            ],
        },
        {
            "t": "table",
            "h2": "どの部分を移すべきか",
            "intro": "ワークロードごとに、正直に書きます。最後の 3 行は、よく間違われるところです。",
            "head": ["ワークロード", "移すべきか", "理由"],
            "rows": [
                ["セッション、トークン", "*はい", "TTL つきの、キーによる参照です。データベースは仕事としてではなく、厚意でやってくれていました。"],
                ["レート制限、カウンタ", "*はい", "期限つきの INCR は原子的で O(1) です。SQL では、最もホットな行に対する行ロックになります。"],
                ["ジョブキュー", "*はい", "リストとストリーム。コンシューマグループと、メッセージごとの確認応答があります。キュー用のテーブルは、手数の増えたロックの慣習にすぎません。"],
                ["フィーチャーフラグ、設定", "*はい", "絶えず読まれ、めったに書かれず、結合されることはありません。"],
                ["絞り込んだ一覧(状態や所有者で)", "*多くの場合", "セカンダリインデックスが、クエリプランナなしで答えます。<a href=\"~/use/app-store/\">読みを捌く</a>を参照してください。"],
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
                "も参照に変えます。",
                "<b>読み取りが本当にクエリなら、kevy を使わないでください。</b>5 つの"
                "テーブルにまたがる結合、アドホックな分析、無関係な行にまたがり本物の"
                "分離レベルを要求するトランザクション——それは PostgreSQL の仕事であり、"
                "PostgreSQL に留めておくべきです。<a href=\"~/docs/rds-workloads/\">"
                "リレーショナルな各ワークロードが、ここでいくらかかるか</a>を書いて"
                "あります。答えが「やめておきなさい」になるものも含めて。",
                "<b>1 台で足りないなら、kevy を使わないでください。</b>クラスタモード"
                "はなく、今後もできません。1 台の kevy は毎秒数百万回の操作をこなし、"
                "たいていの製品が到達する上限よりも余裕がありますが、それを超えたら"
                "シャーディングするものが必要です。kevy は、それではありません。",
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
                 "151 KB の WebAssembly。本物の TTL と pub/sub があり、ブラウザのファイルシステムに永続化されます。オフラインでも動きます。"],
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
                    "a": "ワイヤの上では、なります。RESP2 と RESP3、184 個のコマンドに対応し、クライアントライブラリは違いに気づきません。挙動もおおむね同じですが、その例外こそが要点です。<code>SCAN</code> はカーソルによる反復ではありません。1 回の呼び出しでキースペース全体を走査してカーソル 0 を返すので、いつもの SCAN ループは 1 往復で終わります。<code>ZRANK</code> は O(N) です。ソート済みセットに順位の索引を持たないからです。<code>SPOP</code> と <code>SRANDMEMBER</code> はランダムではなく、毎回同じ要素を返します。シャードをまたぐ <code>RENAME</code> は原子的ではありません。<a href=\"~/docs/commands/\">184 個すべてのコマンドに、本当の差異と本当のコストを併記してあります</a>。Redis の文書から書き写したものではなく、実装から読み出したものです。",
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
                    "q": "依存ゼロ——それは無謀ではありませんか",
                    "a": "飾りとは正反対で、可搬性を本物にしているのが、まさにそれです。ハッシュマップ、ハッシュ関数、RESP パーサ、B-tree、arena アロケータ、io_uring のバインディング、Lua インタプリタ——すべてこのリポジトリにあり、すべて Rust です。kevy の周辺にある C は、カーネルがそれ以外の方法では公開していない、ごく少数のシステムコールだけで、<code>kevy-sys</code> に手で書いてあります。おかげでサプライチェーンは、半日で読み切れます。",
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
    "desc": "kevy でのセッション、ホットな行、レート制限、フィーチャーフラグ。なぜ向いているのか、何を差し出すのか、そしてそのためのコマンド。",
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
                "落とします。",
            ],
        },
        {
            "t": "code",
            "h2": "手順",
            "caption": "以下のコマンドは、すべて実物です。動いている kevy に対して、redis-cli から貼り付けてください。",
            "text": """# a session that cleans itself up
SET session:7f3a '{"user":"ada","role":"admin"}' EX 3600
GET session:7f3a
TTL session:7f3a          -> 3599

# a rate limit: one counter per client, expiring on a window
INCR   rate:203.0.113.7   -> 1
EXPIRE rate:203.0.113.7 60
INCR   rate:203.0.113.7   -> 2      (the window survives)

# feature flags: read constantly, written rarely, joined never
HSET  flags new-checkout on dark-mode on beta-search off
HGET  flags new-checkout  -> "on"
HGETALL flags

# a cached row, invalidated by the writer rather than by a timer
SET   user:881 "$json" EX 300
DEL   user:881            # after you write to Postgres""",
        },
        {
            "t": "callout",
            "kind": "loss",
            "title": "何を差し出すことになるか",
            "body": (
                "キャッシュは真実の 2 つ目のコピーであり、間違うことがあります。"
                "kevy はそれを解決しません——それを解決できるものは、ありません。"
                "<b>タイマーではなく、書き込みで無効化してください</b>。TTL は計画"
                "そのものではなく、最後の保険として置いておくものです。また、複数"
                "キーの <code>MSET</code> や <code>DEL</code> が原子的なのは、ひとつ"
                "のシャードの中だけです。2 つのキーが必ず一緒に変わらなければ"
                "ならないなら、<code>{hashtag}</code> で同じ場所に寄せてください。"
            ),
        },
        {
            "t": "cards",
            "h2": "次に",
            "intro": "",
            "items": [
                {"kicker": "ガイド", "title": "クックブック", "body": "セッション、レート制限、リーダーボード、フィードの実用レシピ。", "go": "読む", "href": "docs/cookbook/"},
                {"kicker": "ガイド", "title": "永続化", "body": "kill -9 で何が残り、fsync の方針が何を代償にするのか。", "go": "読む", "href": "docs/persistence/"},
                {"kicker": "リファレンス", "title": "全コマンド", "body": "184 個のコマンド。それぞれの本当のコストと、Redis との差異つき。", "go": "調べる", "href": "docs/commands/"},
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
            "t": "code",
            "h2": "手順——やり直せる仕事には、リスト",
            "caption": "ワーカーはブロックします。ポーリングのループも、sleep も、殺到もありません。",
            "text": """# producer
LPUSH jobs:email '{"to":"ada@example.com","tpl":"welcome"}'

# worker — blocks until there is something, up to 30 seconds
BRPOP jobs:email 30
-> 1) "jobs:email"
   2) "{\\"to\\":\\"ada@example.com\\",\\"tpl\\":\\"welcome\\"}"

# a delayed job: the score is when it is due.
# ZPOPMIN.BELOW is kevy's own — it takes only what is actually due,
# and stops at the first job that is not.
ZADD jobs:due 1783875499 '{"id":"j-91"}'
ZPOPMIN.BELOW jobs:due 1783875500
-> 1) the job payload
   2) 1783875499
""",
        },
        {
            "t": "code",
            "h2": "手順——失えない仕事には、ストリーム",
            "caption": "メッセージは、ワーカーが確認応答するまで保留されます。落ちたワーカーのジョブは、引き取れます。",
            "text": """# once, at setup
XGROUP CREATE jobs:pay g1 $ MKSTREAM

# producer
XADD jobs:pay * order 4410 amount 8400

# worker: read, then work, then acknowledge
XREADGROUP GROUP g1 worker-3 COUNT 1 BLOCK 5000 STREAMS jobs:pay >
XACK jobs:pay g1 1783875499458-0

# the worker died before XACK. another one takes over:
XAUTOCLAIM jobs:pay g1 worker-7 60000 0-0
# claims anything idle for more than 60 s

# what is still outstanding, and who has it
XPENDING jobs:pay g1""",
        },
        {
            "t": "callout",
            "kind": "loss",
            "title": "何を差し出すことになるか",
            "body": (
                "<b>ストリームは、ただではありません。</b><code>MAXLEN</code> による"
                "切り詰めはストリームの重みを計算し直すため、ストリーム全体に対して "
                "O(N) です。<code>XADD</code> のたびではなく、定期的に切り詰めて"
                "ください。<code>XREADGROUP</code> の <code>COUNT</code> が制限するの"
                "は<b>渡される量であって、走査される量ではありません</b>。未配信の"
                "末尾は、まず全体が実体化されます。またマルチシャードのサーバーでは、"
                "複数のキーにまたがる <code>BLPOP</code> は、Redis の厳密な左から右へ"
                "の優先順を守りません。接続自身のシャードにあるキーが、先に処理され"
                "ます。これらはすべて<a href=\"~/docs/commands/\">リファレンス</a>"
                "に、コマンドごとに書いてあります。"
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
            "t": "code",
            "h2": "手順",
            "caption": "チャネルと、その一族をまとめて扱うためのパターン。",
            "text": """# subscriber
SUBSCRIBE room:42
PSUBSCRIBE room:*          # every room, one connection

# publisher — returns how many subscribers received it
PUBLISH room:42 '{"user":"ada","text":"hello"}'
-> (integer) 3

# presence: the TTL does the expiry, the client refreshes every 10 s
SET presence:ada online EX 30

# who is here. on a large keyspace prefer a set:
SADD online ada
SMEMBERS online
SREM online ada""",
        },
        {
            "t": "code",
            "h2": "同じことを、ブラウザのタブで",
            "caption": "同一オリジンの 2 つのタブ。サーバーも WebSocket もありません。橋渡しは BroadcastChannel で、絞り込みはエンジンの中で行われます。",
            "text": """import { open } from "@goliajp/kevy";

const db = await open({ persist: { name: "app" } });

// tab A
db.subscribe("room:42", (payload, channel) => {
  render(JSON.parse(new TextDecoder().decode(payload)));
});

// tab B — tab A receives it
db.publish("room:42", JSON.stringify({ user: "ada", text: "hello" }));""",
        },
        {
            "t": "callout",
            "kind": "loss",
            "title": "何を差し出すことになるか",
            "body": (
                "<b>遅い購読者は、いつまでも待たれるのではなく、切り捨てられます。</b>"
                "クライアントが追いつけない場合、そのメッセージは、サーバーのメモリを"
                "際限なく増やす代わりに破棄されます。意図した選択であり、配信を"
                "当てにする前に知っておくべきことです。確認応答も、再送もありません。"
                "<b>そのどちらかが必要なら、必要なのはチャネルではなくストリーム"
                "です。</b><a href=\"~/docs/pubsub/\">pub/sub のガイド</a>に、"
                "限界を具体的に書いてあります。"
            ),
        },
        {
            "t": "cards",
            "h2": "次に",
            "intro": "",
            "items": [
                {"kicker": "ガイド", "title": "pub/sub", "body": "チャネル、パターン、そして遅れた購読者に何が起きるのか。", "go": "読む", "href": "docs/pubsub/"},
                {"kicker": "試す", "title": "2 つのタブ、サーバーなし", "body": "playground を 2 つのタブで開いて、どちらからでも publish してみてください。", "go": "Playground", "href": "play/"},
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
                "どのキーの、どのフィールドかをエンジンに伝えれば、書き込みの側が"
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
            "t": "code",
            "h2": "手順——ベクトル検索",
            "caption": "キーのフィールドに張る HNSW インデックス。一度宣言すれば、あとは書き込みの側が最新に保ちます。",
            "text": """# declare it once. the engine backfills, and answers
# INDEXBUILDING while it does.
IDX.CREATE idx:sem ON PREFIX doc: FIELD vec TYPE vector KIND ann  DIM 768 DISTANCE cosine M 16 EF 200

# write a document the way you already write documents
HSET doc:4410 title "Ada on pipelining" vec "<768 f32, little-endian>"

# nearest ten. no separate system, no sync step.
IDX.QUERY idx:sem KNN "<query vector>" LIMIT 10
-> 1) doc:4410
   2) doc:9982""",
        },
        {
            "t": "code",
            "h2": "手順——全文検索、そして両者の併用",
            "caption": "同じキーに対する BM25、両方のランキングを融合するハイブリッドクエリ、そして追いかけられるフィード。",
            "text": """IDX.CREATE idx:ft ON PREFIX doc: FIELD title TYPE str KIND text

IDX.QUERY idx:ft MATCH "pipelining"
-> 1) 1) "doc:1"
      2) "0.2877"          # the BM25 score

# hybrid: fuse the text ranking and the vector ranking (RRF)
IDX.QUERY HYBRID idx:ft MATCH "pipelining" idx:sem KNN "<vector>"  LIMIT 20 RRFK 60

# a change feed: tail every write from another process.
# needs [feed] enabled = true in kevy.toml
FEED.SHARDS                 -> (integer) 16
FEED.TAIL 0                 -> 1) (integer) 1     # generation
                               2) (integer) 1     # offset
FEED.READ 0 1 0 COUNT 2     -> the writes themselves, replayable""",
        },
        {
            "t": "callout",
            "kind": "loss",
            "title": "何を差し出すことになるか",
            "body": (
                "<b>インデックスの構築は、対象となるキーに対して O(N) です。</b>"
                "追いつくまで、インデックスは <code>INDEXBUILDING</code> を返します。"
                "最初の構築は、行き当たるのではなく計画してください。<b>ベクトル"
                "インデックスは HNSW であり、近似です</b>。再現率は保証ではなく、"
                "調整のためのパラメータ(<code>EF</code>)です。そして<b>埋め込み"
                "モデルはありません</b>。kevy が代わりに呼んでくれることを期待して"
                "いたなら、それは起きません。計画を立てる前に、知っておいてください。"
                "<a href=\"~/docs/vector-search/\">ベクトルのガイド</a>と"
                "<a href=\"~/docs/text-search/\">テキストのガイド</a>に、"
                "具体的に書いてあります。"
            ),
        },
        {
            "t": "callout",
            "kind": "note",
            "title": "これを読んでいるのがエージェントなら",
            "body": (
                "<a href=\"~/llms-full.txt\">llms-full.txt</a> は、1 回の取得で"
                "済みます。全コマンドと、その本当のコストと Redis との本当の差異、"
                "そして 24 本のガイド全文が入っています。エンジン自身のコマンド表から"
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
                "パターンとフィールドを指定すれば、書き込みの側が、それを最新に保ち"
                "ます。絞り込んだ一覧は、ふたたび参照になります。プランナもスキャンも"
                "クエリもありません。",
                "<b>ビューはさらに進んで</b>、書き込み時に集計を最新に保ちます。件数や"
                "合計は、計算されるのではなく読まれます。ほとんどのアプリケーションが "
                "ORM に実際に求めているのはこれであり、データベースが忙しい理由も、"
                "これです。",
            ],
        },
        {
            "t": "code",
            "h2": "手順",
            "caption": "インデックスを宣言し、普通に書き、フィールドで読む。ここにあるコマンドはすべて、実際のサーバーに対して実行したものです。",
            "text": """# your data, written the way you would anyway
HSET order:1001 customer 881 status open  total 4400
HSET order:1002 customer 881 status paid  total 8400
HSET order:1003 customer 902 status open  total 1200

# one index per field you want to look up by
IDX.CREATE idx:cust   ON PREFIX order: FIELD customer TYPE i64 KIND range
IDX.CREATE idx:status ON PREFIX order: FIELD status   TYPE str KIND range

# the read that would have been a query
IDX.QUERY idx:cust EQ 881
-> 1) "0"                       # cursor
   2) 1) "order:1001"  2) "881"
      3) "order:1002"  4) "881"

# two conditions at once
IDX.QUERY COMPOSE AND idx:cust EQ 881 idx:status EQ open
-> 1) "0"
   2) 1) 1) "order:1001"

# a VIEW keeps the answer ready on the WRITE path, so the read
# never recomputes it. (the parens are separate arguments.)
VIEW.CREATE v:open881 QUERY ( AND idx:cust EQ 881 idx:status EQ open )  ORDER BY idx:cust
VIEW.QUERY  v:open881
-> 1) "0"
   2) 1) "order:1001"  2) "881"
""",
        },
        {
            "t": "callout",
            "kind": "loss",
            "title": "何を差し出すことになるか",
            "body": (
                "<b>インデックスとビューの代金は、読み取りではなく、書き込みのたびに"
                "支払います。</b>それが取引の条件であり、読み取り主体の配信には"
                "正しく、書き込み主体のログには間違っています。<b>結合はありません</b>"
                "し、今後も持ちません。インデックスが答えるのは「どのキーがこれらの"
                "フィールドに合致するか」であって、「この 2 つのコレクションを結合"
                "せよ」ではありません。読み取りに本当に結合が要るなら、Postgres に"
                "置いておいてください。<a href=\"~/docs/rds-workloads/\">"
                "リレーショナルな各ワークロードが、ここでいくらかかるか</a>を書いて"
                "あります。答えが「移さないほうがいい」になるものも含めて。"
            ),
        },
        {
            "t": "cards",
            "h2": "次に",
            "intro": "",
            "items": [
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
                "struct であり、151 KB の WebAssembly モジュールであり、OS のない"
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
            "t": "code",
            "h2": "手順——Rust のプログラムの中で",
            "caption": "ソケットも、シリアライズも、2 つ目のプロセスもありません。永続化され、開くときにログを再生します。",
            "text": """kevy-embedded = "4.0"

let db = Db::open("data/")?;
db.set(b"session:7f3a", b"{\\"user\\":\\"ada\\"}", Some(Duration::from_secs(3600)))?;
assert_eq!(db.get(b"session:7f3a")?.is_some(), true);

// need other processes to reach it later? open the RESP listener
// and your redis-cli works, without changing any of the above.
db.listen("127.0.0.1:6379")?;""",
        },
        {
            "t": "code",
            "h2": "手順——ブラウザのタブで",
            "caption": "gzip 後 151 KB。ブラウザ自身のファイルシステムに永続化され、リロードにも耐え、タブをまたいで pub/sub を話します。",
            "text": """import { open } from "@goliajp/kevy";

const db = await open({ persist: { name: "app" } });

db.set("cart:u881", JSON.stringify(items), { ttlMs: 86_400_000 });
db.get("cart:u881");        // still there after a reload
db.pttl("cart:u881");       // the engine expires it, not your code

db.subscribe("sync", (payload) => merge(payload));   // other tabs""",
        },
        {
            "t": "code",
            "h2": "手順——マイコンの上で",
            "caption": "no_std、アロケータなし、OS なし。大きさを自分で決める、固定の arena。",
            "text": """# Cargo.toml
kevy-store = { version = "4.0", default-features = false }

# no_std, no heap: the store lives in an arena you provide
let mut arena = [0u8; 64 * 1024];
let mut store = Store::new_in(&mut arena);
store.set(b"temp", b"21.4")?;""",
        },
        {
            "t": "callout",
            "kind": "loss",
            "title": "何を差し出すことになるか",
            "body": (
                "<b>ブラウザでは、小さな同期の読み取りなら localStorage のほうが"
                "速いです</b>——あれはページ自身のアドレス空間にあるマップであり、"
                "OPFS の上に作られたものが、そこで勝つことはありません。kevy が"
                "勝つのは、そもそも localStorage を選ぶべきでない理由のほうです。"
                "本物の TTL、5 MB の上限がないこと、値が文字列ではなくバイト列で"
                "あること、書き込みがメインスレッドを止めないこと。<b>マイコンでは、"
                "arena の大きさを自分で決めます</b>し、実行中に広げることはできません。"
                "アロケータがないとは、そういうことです。そして<b>組み込みのストアは"
                "共有されません</b>。2 つ目のプロセスがデータを必要とするなら、必要な"
                "のはサーバーか、組み込みの RESP リスナーです。"
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
            "head": ["", "kevy 4.0", "Redis 8", "valkey 9.1", "Dragonfly", "Redis 8 比"],
            "rows": [
                ["GET", "7,800,299", "5,597,865", "3,014,687", "2,132,210", "*1.39×"],
                ["SET", "6,918,058", "2,573,396", "1,749,976", "1,511,377", "*2.69×"],
                ["INCR", "6,133,940", "3,459,395", "2,484,273", "1,387,568", "*1.77×"],
                ["SADD", "5,600,597", "3,690,483", "2,385,857", "1,678,098", "*1.52×"],
                ["HSET", "4,287,217", "3,021,325", "1,970,791", "1,515,763", "*1.42×"],
                ["LPUSH", "3,213,470", "2,862,374", "1,943,222", "1,320,497", "!1.12×"],
                ["ZADD", "3,053,101", "2,773,929", "1,802,759", "1,455,126", "!1.10×"],
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
                ["kevy.wasm", "416 KB", "エンジン本体、非圧縮"],
                ["gzip 後", "151 KB", "回線を流れる量"],
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

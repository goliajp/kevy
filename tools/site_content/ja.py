# Japanese content for the marketing and scenario pages.
#
# Written as Japanese, not translated from en.py. The English is the source
# material for the *facts*; the sentences are rebuilt so they land the way
# Japanese technical prose lands — the conclusion at the end, short declaratives
# stacked, plain form (da/de-aru) throughout. Reference register: the Japanese
# edition of the Rust book, the PostgreSQL Japanese manual. Not a brochure.
#
# Numbers, code samples, hrefs and the */! table prefixes are byte-identical to
# en.py on purpose: gen_site.py refuses a page or block that drifts, and the
# table's honesty depends on the ! rows painting the same colour in every
# language.
#
# Punctuation is gated. Japanese prose takes 「、」「。」「:」「(」「)」「——」 and
# never an ASCII `,` or `.` next to a Japanese character. ASCII is correct inside
# <code>, inside code samples, and in numbers. Run:
#   python3 tools/check_cjk_punct.py tools/site_content/ja.py

PAGES = {}

# ── / ───────────────────────────────────────────────────────────────────────

PAGES[""] = {
    "title": "kevy — 純 Rust で書いた Redis 互換エンジン",
    "desc": "サードパーティ製クレートを一つも使わず、純 Rust で書いた Redis 互換のストレージエンジン。サーバーとして動き、バイナリに組み込め、WebAssembly にコンパイルでき、マイコンにも収まる。",
    "foot": "純 Rust、サードパーティ依存ゼロ",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "バージョン 4.0",
            "h1": "<span class=\"nb\">Redis 互換</span>のエンジンを、<br>ゼロから書いた。",
            "lede": (
                "kevy は RESP を話し、184 個のコマンドに応答する。そして<b>サード"
                "パーティ依存を一つも持たない</b>——ハッシュマップの crate も、ハッシュ"
                "関数も、非同期ランタイムも入っていない。crate は 33 個、すべて自前、"
                "すべて Rust。同じエンジンが 16 コアのサーバーを動かし、CLI に組み込まれ、"
                "151 KB の WebAssembly にコンパイルされ、アロケータすら持たない "
                "Cortex-M マイコンの上で起動する。"
            ),
            "ctas": [
                {"label": "ブラウザで試す", "href": "play/"},
                {"label": "ドキュメントを読む", "href": "docs/"},
                {"label": "数字を見る", "href": "benchmarks/"},
            ],
            "aside": """<pre class="hero-code"><code>$ cargo install kevy
$ kevy --port 6379

$ redis-cli -p 6379
&gt; SET greeting "hello"
OK
&gt; INFO server
kevy_version:4.0.0
dependencies:0</code></pre>""",
        },
        {
            "t": "cards",
            "h2": "同じエンジンを、どこに置くか",
            "intro": "データがどこにあるべきかで選べばいい。API はどれも同じだ。",
            "items": [
                {
                    "kicker": "サーバー",
                    "title": "Redis をそのまま置き換える",
                    "body": "1 コアにつき 1 シャード、Linux では io_uring、SO_REUSEPORT。既存のクライアントは、入れ替わったことに気づかない。",
                    "go": "サーバーを動かす",
                    "href": "docs/tuning/",
                },
                {
                    "kicker": "組み込み",
                    "title": "バイナリの中に置くストア",
                    "body": "ソケットもプロセスもシリアライズもない。永続化される HashMap のつもりで呼べばいい。",
                    "go": "組み込む",
                    "href": "docs/embedded-listener/",
                },
                {
                    "kicker": "ブラウザ",
                    "title": "151 KB の WebAssembly",
                    "body": "TTL と pub/sub を備えた本物のキースペースがタブの中で動き、OPFS に永続化される。localStorage のラッパーではない。",
                    "go": "Playground を開く",
                    "href": "play/",
                },
                {
                    "kicker": "エッジ",
                    "title": "worker のコールドスタートに間に合う",
                    "body": "暖機するランタイムも、張りにいくコネクションもない。ストアはコードと同じ isolate の中にある。",
                    "go": "エッジに載せる",
                    "href": "docs/wasm/",
                },
                {
                    "kicker": "ベアメタル",
                    "title": "アロケータもなし、OS もなし",
                    "body": "kevy-store は no_std。固定 arena だけを使い、ヒープなしで Cortex-M の上を走る——CI が push のたびに証明している。",
                    "go": "MCU プローブを見る",
                    "href": "docs/iot/",
                },
                {
                    "kicker": "エージェント",
                    "title": "LLM の記憶",
                    "body": "ベクトル索引と全文索引、変更フィード、そしてエンジン自身のコマンド表から起こした llms.txt がある。",
                    "go": "llms.txt を読む",
                    "href": "llms.txt",
                },
            ],
        },
        {
            "t": "table",
            "h2": "スループットを、正直に測る",
            "intro": (
                "1 台のマシン、16 コア、ループバック。5 回実行した中央値で、サーバー"
                "自身の <code>total_commands_processed</code> を 3 秒間の定常状態で"
                "数えた値だ。redis-benchmark が表示する rps は<b>使っていない</b>"
                "——あれは 250 ms に量子化されていて、こちらに都合のいい嘘をつく。"
            ),
            "head": ["", "kevy 4.0", "valkey 9.1", "Redis 8", "Dragonfly", "Redis 8 比"],
            "rows": [
                ["GET", "7,800,299", "3,014,687", "5,597,865", "2,132,210", "*1.39×"],
                ["SET", "6,918,058", "1,749,976", "2,573,396", "1,511,377", "*2.69×"],
                ["INCR", "6,133,940", "2,484,273", "3,459,395", "1,387,568", "*1.77×"],
                ["SADD", "5,600,597", "2,385,857", "3,690,483", "1,678,098", "*1.52×"],
                ["HSET", "4,287,217", "1,970,791", "3,021,325", "1,515,763", "*1.42×"],
                ["LPUSH", "3,213,470", "1,943,222", "2,862,374", "1,320,497", "!1.12×"],
                ["ZADD", "3,053,101", "1,802,759", "2,773,929", "1,455,126", "!1.10×"],
            ],
            "note": (
                "3 者すべてに 7 戦 7 勝。ただし、最後の 2 行を読んでほしい。Redis 8 に"
                "対して LPUSH は 12%、ZADD は 10% しか上回っていない——この差では、"
                "勝者を決めるのはこちらではなく、そちらのハードウェア、値のサイズ、"
                "キーの分布だ。この 2 行には、差が薄いときのためにとってある色を塗って"
                "ある。都合のいい行だけを並べたベンチマークは、ただの広告だからだ。"
                "<a href=\"benchmarks/\">測定方法の全体と、それを正しくするために"
                "払った代償。</a>"
            ),
        },
        {
            "t": "prose",
            "h2": "依存ゼロは設計上の制約であって、自慢ではない",
            "body": [
                "ハッシュマップは自前だ。ハッシュ関数も自前だ。RESP パーサ、B-tree、"
                "arena アロケータ、io_uring のバインディング、イベントループ、geohash、"
                "Lua インタプリタ——すべて自前で、すべて Rust で、すべてこのリポジトリの"
                "中にある。kevy の周辺に存在する C は、カーネルがそれ以外の方法では"
                "見せてくれない、ごく少数のシステムコールだけだ。それも "
                "<code>kevy-sys</code> の中に <code>unsafe extern \"C\"</code> として"
                "手で書いてある。libc の crate はリンクしていない。",
                "純粋さのための純粋さではない。同じコードが 16 コアのサーバーにも、"
                "no_std のマイコンにも、WebAssembly モジュールにもコンパイルできるのは、"
                "これがあるからだ——依存ツリーの中に、それは無理だと言ってくる相手が"
                "いない。アロケータやスレッドや時計の存在を前提にする crate は、"
                "そのたびに扉をひとつ閉める。",
                "そして、サプライチェーンが読めるものになる。crate は 33 個、作者は "
                "1 人、<code>cargo tree</code> は画面 1 枚に収まる。",
            ],
        },
        {
            "t": "callout",
            "kind": "loss",
            "title": "kevy がやらないこと",
            "body": (
                "クラスタではない。gossip もスロット移行も sentinel もない。"
                "レプリケーションとフェイルオーバーはあるが、マシンをまたぐ"
                "シャーディングはない。これは<a href=\"docs/cluster/\">意図的に断って"
                "いる</a>。AUTH も TLS もない。それらをきちんと処理できるものの後ろに"
                "置くこと。Redis と挙動の違うコマンドがいくつかあり、中には意外な"
                "ものもある——<code>SCAN</code> はカーソルによる反復ではない。"
                "<code>ZRANK</code> は O(N) だ。<code>SPOP</code> はランダムではない。"
                "<a href=\"docs/commands/\">その一つひとつを書き出してある</a>"
                "——Redis 自身のリファレンスには存在しない欄として。"
            ),
        },
        {
            "t": "steps",
            "h2": "30 秒",
            "intro": "入口は 3 つ。データがどこにあるかで選べばいい。",
            "items": [
                {
                    "title": "サーバーとして",
                    "body": "6379 番で RESP を話す。redis-cli も、クライアントライブラリも、既存のコードもそのまま使える。",
                    "code": "cargo install kevy\nkevy --port 6379",
                },
                {
                    "title": "Rust のバイナリの中で",
                    "body": "ソケットもプロセスもない。エンジンは struct だ。",
                    "code": 'kevy-embedded = "4.0"\n\nlet db = Db::open("data/")?;\ndb.set(b"k", b"v", None)?;\nassert_eq!(db.get(b"k")?.as_deref(), Some(&b"v"[..]));',
                },
                {
                    "title": "ブラウザのタブの中で",
                    "body": "gzip 後 151 KB。OPFS に永続化され、リロードしても残り、タブをまたいで pub/sub を話す。",
                    "code": 'import { open } from "@goliajp/kevy";\n\nconst db = await open({ persist: { name: "app" } });\ndb.set("cart:u1", JSON.stringify(items), { ttlMs: 3600_000 });',
                },
            ],
        },
    ],
}

# ── /benchmarks/ ────────────────────────────────────────────────────────────

PAGES["benchmarks"] = {
    "title": "ベンチマーク — kevy",
    "desc": "1 台のマシンで kevy 4.0 を valkey 9.1、Redis 8、Dragonfly と比べた。測定方法と数字、そして計測ハーネス自身が嘘をついていたと気づくまでに費やした 1 週間の話。",
    "foot": "このページの数字はすべて bench/ から再現できる",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "ベンチマーク",
            "h1": "数字と、それを最初に間違えた話",
            "lede": (
                "ここにある数字はすべて、リポジトリの <code>bench/</code> から再現できる。"
                "それよりも役に立つ話をする。以前の数字を無価値にした間違いが何だったか、"
                "どうやってそれに気づいたか、二度と起きないように何を変えたか。"
            ),
        },
        {
            "t": "callout",
            "kind": "warn",
            "title": "計測ハーネスは、私たちに嘘をついていた",
            "body": (
                "redis-benchmark は、250 ms ごとに発火するタイマー"
                "(<code>SHOW_THROUGHPUT_INTERVAL</code>)を使ってスループットを出す。"
                "だから表示される rps は、必ず <code>n / (0.25 秒の整数倍)</code> に"
                "なる——そして kevy が出す速度では、1 回の実行が数ティックで終わって"
                "しまう。以前の表で GET と SET が一桁まで同じ数字を出していたのは、"
                "これが理由だ。私たちはその不自然さを、肩をすくめて見過ごしていた。"
                "あれは量子化だった。<code>redis-benchmark.c</code> を読むまでの "
                "1 週間、ありもしない 5% の性能低下を追いかけていた。いま載せている"
                "数字は、すべてサーバー自身の <code>INFO stats</code> を 3 秒間の"
                "定常状態で数えたものだ。さらにハーネスは参照コミットを毎回ビルドし"
                "直し、2 つを交互に走らせる。マシンの状態がドリフトしても、それだけで"
                "結果を作れないようにするためだ。"
            ),
        },
        {
            "t": "table",
            "h2": "同じ 1 台で、4 つを比べる",
            "intro": (
                "16 コア、loopback、50 コネクション、3 バイトの value。5 回の実行の中央値。5 回のあいだのばらつきは"
                "<a href=\"https://github.com/goliajp/kevy/blob/main/bench/PERF-LEDGER.md\">台帳</a>にある —— "
                "どの行も数パーセントであり、順位を変えるほどではない。"
            ),
            "head": ["", "kevy 4.0", "valkey 9.1", "Redis 8", "Dragonfly", "valkey 比", "Redis 8 比", "Dragonfly 比"],
            "rows": [
                ["GET", "7,800,299", "3,014,687", "5,597,865", "2,132,210", "*2.59×", "*1.39×", "*3.66×"],
                ["SET", "6,918,058", "1,749,976", "2,573,396", "1,511,377", "*3.95×", "*2.69×", "*4.58×"],
                ["INCR", "6,133,940", "2,484,273", "3,459,395", "1,387,568", "*2.47×", "*1.77×", "*4.42×"],
                ["SADD", "5,600,597", "2,385,857", "3,690,483", "1,678,098", "*2.35×", "*1.52×", "*3.34×"],
                ["HSET", "4,287,217", "1,970,791", "3,021,325", "1,515,763", "*2.18×", "*1.42×", "*2.83×"],
                ["LPUSH", "3,213,470", "1,943,222", "2,862,374", "1,320,497", "*1.65×", "!1.12×", "*2.43×"],
                ["ZADD", "3,053,101", "1,802,759", "2,773,929", "1,455,126", "*1.69×", "!1.10×", "*2.10×"],
            ],
            "note": (
                "7 戦 7 勝。ただし<b>LPUSH は Redis 8 より 12%、ZADD は 10% しか"
                "速くない</b>——この程度の差では、エンジンよりも値のサイズとキーの"
                "分布のほうが効く。その 2 つが自分のホットなコマンドなら、うちの数字を"
                "鵜呑みにせず、自分のワークロードで測ってほしい。色が付いているのには"
                "意味がある。余裕を持って勝てていない行にだけ、この色を使う。この表を"
                "ひと目見たときに、見出しではなく事実のほうが伝わるように。"
            ),
        },
        {
            "t": "prose",
            "h2": "このベンチマークが教えてくれないこと",
            "body": [
                "これは 1 台のマシンの、ループバック上の、小さな値による測定だ。"
                "つまりネットワークが消えている。実際の運用で本当に待たされている"
                "相手は、たいていそのネットワークのほうだ。GET が valkey より "
                "2.6 倍速くても、レイテンシの 90% が回線なら、p99 が 2.6 倍良く"
                "なることはない。",
                "値が大きくなると、話の形が変わる。1 値あたり 64 KB では全体が"
                "カーネルの TCP パスに律速され、差は 1 桁台まで縮まる。perf の"
                "トレースと書き起こしは <code>bench/</code> に置いてある——その中には、"
                "ユーザー空間の memcpy を実測で減らしたのに、スループットがまったく"
                "動かなかった最適化が 3 件含まれている。memcpy はボトルネックでは"
                "なく、ただの税だったからだ。",
                "エンジンは単一ノードだ。1 台では足りないという問題を抱えているなら、"
                "kevy はそれを解決しない。このページのどの数字も、その事実を変えない。",
            ],
        },
        {
            "t": "table",
            "h2": "ブラウザ向けビルド",
            "intro": "タブに実際に配るもの。",
            "head": ["", "サイズ", "備考"],
            "rows": [
                ["kevy.wasm", "416 KB", "エンジン本体、非圧縮"],
                ["gzip 後", "151 KB", "回線を流れる量"],
                ["コールドスタート", "&lt; 20 ms", "コンパイルとインスタンス化、キャッシュが温まった状態"],
            ],
            "note": (
                "小さな読み取りだけを比べれば、localStorage のほうが速い。"
                "あれはブラウザ自身のアドレス空間にある同期のマップであり、OPFS の上に"
                "作られたものがそこで勝つことはない。kevy が勝つのは、そもそも "
                "localStorage を選ぶべきでない理由のほうだ：TTL があること、5 MB の"
                "上限がないこと、値が文字列ではなくバイト列であること、書き込みが"
                "メインスレッドを止めないこと。"
            ),
        },
        {
            "t": "code",
            "h2": "再現する",
            "caption": "このページの内容は、すべてこの 2 つのスクリプトから出てくる。",
            "text": "git clone https://github.com/goliajp/kevy && cd kevy\n\n# four-way arena: kevy, valkey, redis 8, dragonfly\nbash bench/arena.sh\n\n# the regression gate: rebuilds the reference commit and interleaves\nbash bench/perfgate.sh",
        },
    ],
}

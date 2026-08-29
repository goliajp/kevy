# Chinese content for kevy.golia.jp.
#
# kevy is the data layer for building AI systems. Redis-compatible, faster, and
# it covers what a Redis plus a vector database plus a search index plus a queue
# were covering between them.
#
# Written in Chinese, not translated. en.py is the source material and the
# schema — same pages, same blocks, same rows, same numbers, same code — but the
# sentences are rebuilt.
#
# The site says two things: what the reader needs to see, and what we actually
# are. It does not talk about our engineering — not the dependency count, not the
# language, not how carefully we measured. Measuring honestly is the floor, not an
# achievement, and a page that congratulates itself for it is a page about itself.
#
# What DOES belong, because it changes what a reader decides: where we are only
# barely ahead (LPUSH: 12%), what we refuse to do (no cluster, no AUTH, no TLS),
# and which commands do not behave the way Redis's docs say. Keep every word of
# that, blunt.
#
# Punctuation: Chinese prose takes full-width marks only. 「，」 for a clause,
# 「、」 between list items, 「。」 to end a sentence, and a 破折号 with no space on
# either side. ASCII punctuation belongs to code, to inline code spans, and to
# numbers. tools/check_cjk_punct.py refuses the file otherwise.
#
# Do not write a bare opening code tag in a comment here. The gate blanks out
# everything between one and the next closing tag, and an unclosed one in this
# header once swallowed the first thirty lines of content — a half-width comma sat
# in the blanked region and the gate reported ok.
#
# Every "code" and "text" field is byte-identical to en.py: those commands are
# executed against a real server by tools/check_site_commands.py.

PAGES = {}

# ── / ───────────────────────────────────────────────────────────────────────

PAGES[""] = {
    "title": "kevy——AI 系统的数据层",
    "desc": "一个为 AI 系统准备的 Redis 兼容数据层：协议不变，吞吐更高，向量检索、全文检索、索引、视图和变更流都在一个引擎里。现场试一下——这一页上的终端就是真的引擎，跑在你的标签页里。",
    "foot": "GOLIA",
    "blocks": [
        {
            "t": "hero",
            "h1": "AI 系统的<br>数据层。",
            "lede": (
                "兼容 Redis——你的客户端不用改就能连。每一个操作都更快。"
                "向量检索、全文检索、索引、视图和变更流都<b>在引擎里</b>，"
                "而不是在围着它的四个服务里。<b>这个终端是真的</b>："
                "同一个引擎，编译成 WebAssembly，就跑在这个标签页里。"
            ),
            "ctas": [
                {"label": "cargo install kevy", "href": "#start"},
                {"label": "它能做什么", "href": "#code"},
                {"label": "打开 Playground", "href": "#try"},
            ],
            "live_term": {
                "hint": "输入一条命令——SET、GET、TTL、INCR、KEYS、SUBSCRIBE、PUBLISH……",
                "chips": [
                    'SET session:7f3a \'{"user":"ada"}\' EX 30',
                    "GET session:7f3a",
                    "TTL session:7f3a",
                    "INCR hits",
                    "KEYS *",
                    "SUBSCRIBE news",
                    "PUBLISH news deployed",
                ],
            },
        },
        {
            "t": "tabs",
            "id": "code",
            "tone": "deep",
            "eyebrow": "它还能做什么",
            "h2": "一个引擎。AI 系统需要的整个栈。",
            "intro": "下面每一条命令，在这一页发布之前，都在 CI 里对着一台真实的服务器跑过。逐个点开——用 kevy 就是这个样子。",
            "items": [
                {
                    "label": "向量",
                    "code": """# an HNSW index over your keys — declared once,
# kept current by the write path
IDX.CREATE idx:sem ON PREFIX doc: FIELD vec TYPE vector KIND ann DIM 768 DISTANCE cosine M 16 EF 200

HSET doc:4410 title "Ada on pipelining" vec "<768 f32, little-endian>"

# nearest ten. no separate vector database, no sync job.
IDX.QUERY idx:sem KNN "<query vector>" LIMIT 10
-> 1) "doc:4410"
   2) "doc:9982"
""",
                    "note": "embedding 由你给出；kevy 负责存它、索引它、检索它。引擎里没有模型，这是故意的。",
                    "go": "Agent 记忆与 RAG",
                    "href": "use/ai/",
                },
                {
                    "label": "全文",
                    "code": """IDX.CREATE idx:ft ON PREFIX doc: FIELD title TYPE str KIND text

IDX.QUERY idx:ft MATCH "pipelining"
-> 1) 1) "doc:1"
      2) "0.2877"          # BM25 score

# hybrid: fuse the text ranking with the vector ranking
IDX.QUERY HYBRID idx:ft MATCH "pipelining" idx:sem KNN "<vector>" LIMIT 20 RRFK 60""",
                    "note": "BM25，带中日韩分词，建在向量索引的同一批 key 上。",
                    "go": "检索是怎么工作的",
                    "href": "use/ai/",
                },
                {
                    "label": "索引",
                    "code": """HSET order:1001 customer 881 status open  total 4400
HSET order:1002 customer 881 status paid  total 8400

IDX.CREATE idx:cust   ON PREFIX order: FIELD customer TYPE i64 KIND range
IDX.CREATE idx:status ON PREFIX order: FIELD status   TYPE str KIND range

# the read that would have been a SQL query
IDX.QUERY COMPOSE AND idx:cust EQ 881 idx:status EQ open
-> 1) "0"
   2) 1) 1) "order:1001"
""",
                    "note": "带过滤的读始终是一次查表。没有查询计划器，没有扫描。",
                    "go": "不靠数据库扛住读",
                    "href": "use/app-store/",
                },
                {
                    "label": "视图",
                    "code": """# the answer, kept current by the WRITE path
VIEW.CREATE v:open881 QUERY ( AND idx:cust EQ 881 idx:status EQ open ) ORDER BY idx:cust

VIEW.QUERY v:open881
-> 1) "0"
   2) 1) "order:1001"  2) "881"

# reads never recompute it; writes keep it fresh""",
                    "note": "大多数应用向 ORM 真正索要的，就是这个。",
                    "go": "物化视图",
                    "href": "use/app-store/",
                },
                {
                    "label": "表",
                    "code": """# a table is a declaration — compiled to named indexes, once
TABLE.DECLARE user PREFIX u: PK id COLUMN id str COLUMN name str COLUMN age i64 COLUMN dept str INDEX age range VALUES dept name ORDERPATH by_dept_age ON dept THEN age DESC

HSET u:1 id 1 name ada age 34 dept eng

# the ORDER BY dept, age DESC walk — one composite index, no planner
IDX.QUERY user.by_dept_age WHERE dept EQ eng LIMIT 20 FIELDS name age""",
                    "note": "类型化列、二级索引、复合 ORDER BY 路径——连你的 PG/MySQL schema 文件也能编译（kevy-cli sql compile）。没有运行期 SQL，没有 join：那些留在 Postgres。",
                    "go": "单表服务型读",
                    "href": "use/app-store/",
                },
                {
                    "label": "大过 RAM",
                    "code": """# kevy.toml — a RAM budget for the whole store
[tiering]
budget = "70%"               # or "4gb", or "auto"

# past the budget, the coldest values spill to a disk log
# and page back on access. a cold key is an ordinary key:
GET archive:2019:q3          # pays one disk read, same reply
TTL archive:2019:q3          # metadata answers from RAM
SCAN 0 MATCH archive:*       # sees cold keys — one key table""",
                    "note": "RAM 决定键的上限，磁盘决定数据的上限；AOF 持久化契约不动。v1 下沉字符串和 hash——list、set、stream 留在热层。",
                    "go": "分层存储怎么工作",
                    "href": "docs/tiering/",
                },
                {
                    "label": "变更流",
                    "code": """# tail every write from another process — or an agent.
# [feed] enabled = true in kevy.toml
FEED.SHARDS                 -> (integer) 16
FEED.TAIL 0                 -> 1) (integer) 1     # generation
                               2) (integer) 1     # offset
FEED.READ 0 1 0 COUNT 2     -> the writes themselves, replayable""",
                    "note": "偏移量可以续上。不用轮询，也不会漏。",
                    "go": "变更流",
                    "href": "use/ai/",
                },
                {
                    "label": "哪里都能放",
                    "code": """# a 16-core server
cargo install kevy && kevy --port 6379

# inside your binary — no socket, no process
let db = Db::open("data/")?;
db.set(b"k", b"v", None)?;

# a browser tab — 481 KB, persists to OPFS
const db = await open({ persist: { name: "app" } });

# a microcontroller — no OS, no allocator
let mut store = Store::new_in(&mut arena);""",
                    "note": "四个地方，同一个引擎、同一批命令。",
                    "go": "把 kevy 嵌进去",
                    "href": "use/embedded/",
                },
            ],
        },
        {
            "t": "bars",
            "id": "swap",
            "eyebrow": "为什么可以直接替换 Redis",
            "h2": "同样的协议。更高的吞吐。",
            "intro": (
                "RESP2 和 RESP3，188 条命令——redis-cli 和你的客户端库不用改就能连。"
                "一台机器，16 核，loopback，五次取中位数。"
            ),
            "rows": [
                ["GET", 7800299, 5597865, "1.39×", False],
                ["SET", 6918058, 2573396, "2.69×", False],
                ["INCR", 6133940, 3459395, "1.77×", False],
                ["SADD", 5600597, 3690483, "1.52×", False],
                ["HSET", 4287217, 3021325, "1.42×", False],
                ["LPUSH", 3213470, 2862374, "1.12×", True],
                ["ZADD", 3053101, 2773929, "1.10×", True],
            ],
            "us": "kevy 6.0.0",
            "them": "Redis 8",
            "thin": "不到 15%——决定胜负的是你的负载，不是引擎",
            "note": (
                "<b>LPUSH 和 ZADD 只领先 12% 和 10%。</b>如果 list 或者 sorted set "
                "是你的热路径，那么性能就不是换过来的理由。"
                "<a href=\"~/benchmarks/\">完整的表格在这里，valkey 和 Dragonfly 也一起打了。</a>"
                "迁移只有三条命令——<a href=\"~/migrate/\">export、import、digest</a>——"
                "而且两个方向都能走。"
            ),
        },
        {
            "t": "steps",
            "id": "start",
            "tone": "deep",
            "h2": "两分钟",
            "intro": "",
            "items": [
                {
                    "title": "安装",
                    "body": "一个二进制。没有运行时，也没有要解析的依赖。",
                    "code": "cargo install kevy\nkevy --port 6379",
                },
                {
                    "title": "把你的客户端指过来",
                    "body": (
                        "你今天在用什么，就继续用什么——没有 kevy 客户端要装。"
                        "<b>node-redis</b> / <b>ioredis</b>、<b>go-redis</b>、"
                        "<b>StackExchange.Redis</b>、<b>redis-py</b>、<b>hiredis</b> "
                        "都能原样连上，kevy 自己的动词走同一个客户端的原始命令通道。"
                        "这六种每次 push 都会在 CI 里对一台真实服务器跑同一套梯子。"
                        "<a href=\"/docs/clients/\">各语言示例</a>（英文）。"
                    ),
                    "code": 'redis-cli -p 6379\n> SET greeting hello\nOK\n> TTL greeting\n(integer) -1',
                },
                {
                    "title": "做一件 Redis 做不到的事",
                    "body": "声明一个索引；写入路径会把它维持在最新。",
                    "code": "IDX.CREATE idx:city ON PREFIX user: FIELD city TYPE str KIND range\nIDX.QUERY  idx:city EQ osaka",
                },
            ],
        },
        {
            "t": "callout",
            "kind": "loss",
            "title": "kevy 不会做的事",
            "body": (
                "<b>它不是集群。</b>复制和故障切换有；把数据分片到多台机器上没有，"
                "以后也不会有。<b>没有 AUTH，也没有 TLS</b>——把它跑在内网，"
                "或者放在一个真正把这两件事做好的东西后面。"
                "<b>多键写只在单个 shard 内原子，不是全局原子</b>——跨 shard 的 "
                "<code>RENAME</code> 或 <code>MSET</code> 不是一步原子完成。"
                "<a href=\"~/docs/commands/\">每一处偏差都按命令逐条写下来了</a>，"
                "<a href=\"~/choose/\">这里还写着什么时候根本不该用它</a>。"
            ),
        },
    ],
}

# ── /migrate/ ───────────────────────────────────────────────────────────────

PAGES["migrate"] = {
    "title": "从 Redis 或数据库迁过来——kevy",
    "desc": "团队为什么会从 Redis 或 Postgres 换到 kevy，具体有哪些变化，代价是什么，以及怎样不重写代码就把迁移做完。",
    "foot": "有什么变化，代价是什么",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "迁移",
            "h1": "你为什么要搬，<br>以及搬过来的代价",
            "lede": (
                "这是两场完全不同的对话。从 <b>Redis</b> 过来，协议是一样的，"
                "要问的是哪些行为不一样。从<b>关系型数据库</b>过来，没有一处是一样的，"
                "要问的是这份负载里到底哪一部分该搬——答案是<b>只搬一部分</b>，"
                "而且我们会说清楚是哪一部分。"
            ),
        },
        {
            "t": "prose",
            "h2": "从 Redis 过来",
            "body": [
                "<b>你的客户端不用改。</b>kevy 说 RESP2 和 RESP3，实现了 188 条命令。"
                "把你现有的库指过来就行，代码不动，redis-cli 不换。没有新的 SDK 要接，"
                "也没有新协议要学。",
                "<b>所以真正要问的只有一句：你能换到什么。</b>只有四样。如果这四样对你都没有"
                "价值，那就留在 Redis——它是一件极好的软件，为换而换，只是白白搭进去一个星期。",
            ],
        },
        {
            "t": "steps",
            "h2": "具体能换到什么",
            "intro": "",
            "items": [
                {
                    "title": "它能跑在 Redis 跑不到的地方",
                    "body": "嵌进二进制，发到浏览器标签页，在一颗没有分配器的 Cortex-M 上启动。今天这些场合各要一层自己的存储、一套自己的 API；在这里，它们是同一个引擎、同一批命令。如果你曾经为客户端那边另写过一个缓存，这就是值得看一眼的理由。",
                },
                {
                    "title": "它顺带把搜索服务也替掉",
                    "body": "二级索引、物化视图、向量 KNN、BM25 全文检索都在引擎里——不是模块，不是 sidecar，也不是一份会慢慢和原数据对不上的副本。原本要跑 Redis 加一个搜索集群的团队，往往只需要跑一个东西。",
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
                    "title": "你现在跑的那些操作，它更快",
                    "body": "同一台机器上对 Redis 8：GET 快 1.4×，SET 快 2.7×，INCR 快 1.8×。不过在你把这个当成理由之前，先把整张表看完——LPUSH 和 ZADD 只领先 12% 和 10%，如果 list 或者 sorted set 是你的热路径，那这就不是你该搬的理由。",
                },
                {
                    "title": "数据集不必再装进 RAM",
                    "body": "给 store 一个 RAM 预算，最冷的值就下沉到磁盘上一份可丢弃的 value log，访问时换回——冷键上每条命令不变，append-only 日志的持久化契约不动。RAM 决定你能放多少个键，磁盘决定你能放多少数据。这替掉的是「Redis 加一个单独的磁盘存储」的拆分——大 value、长尾负载不用再分家。诚实的边界：它默认关闭，v1 只下沉字符串和 hash（list、set、stream 留在热层），64 字节以下的值从不下沉——stub 会和值一样大。",
                    "code": "# kevy.toml\n[tiering]\nbudget = \"70%\"      # or \"4gb\", or \"auto\"",
                },
            ],
        },
        {
            "t": "callout",
            "kind": "loss",
            "title": "离开 Redis 要放弃什么",
            "body": (
                "<b>没有集群。</b>副本是拷贝，不是分片。<b>没有 AUTH，没有 TLS。</b>"
                "还有<b>几条命令的行为不一样</b>——最要紧的一条：多键写只在单个 shard 内原子"
                "（一次调用就把全部返回，游标为 0），跨 shard 的 "
                "<code>RENAME</code> 不是原子的。这些都不是 bug，而且每一条都在命令文档里"
                "写着。请在决定之前把这份清单读完，不要等决定之后："
                "<a href=\"~/docs/commands/\">每条命令真实的代价和真实的偏差</a>。"
            ),
        },
        {
            "t": "code",
            "h2": "怎么从 Redis 搬过来",
            "caption": "从 Redis 导出，导入 kevy，再校验两边一致。下面每一条命令都真的跑过。",
            "text": """# 1. dump what you want to move. it is a RESP file — readable,
#    diffable, and it streams rather than loading into memory.
kevy-cli export -p 6379 --prefix user: dump.resp
-> exported 41023 keys -> dump.resp

# 2. load it. --strict stops on the first error rather than
#    limping onward with a half-migrated keyspace.
kevy-cli import -p 6380 --strict dump.resp
-> imported 82046 ok, 0 errors, offset 4108331

# 3. prove they agree, rather than hoping.
kevy-cli digest -p 6379 user:
kevy-cli digest -p 6380 user:
-> 41023 keys 3bca92aa52269300     # the same hash, or you did not migrate

# an interrupted import resumes where it stopped:
kevy-cli import -p 6380 --resume dump.resp""",
        },
        {
            "t": "prose",
            "h2": "从关系型数据库过来",
            "body": [
                "<b>不要搬你的数据库。</b>要搬的是它身上那一部分从来就不是数据库问题的东西。",
                "会话。限流。功能开关。任务队列。还有那一行每个请求都要读、却从来没人拿去 join "
                "的热点行。在大多数应用里，这些都住在 Postgres 里，而它们正是被敲得最狠的那些"
                "行——不是因为关系型数据库做不好，而是因为它们从来就不是提问。它们是查表。"
                "key 你早就知道了。",
                "<b>Postgres 最擅长的那些事，留给 Postgres</b>——join、临时查询、分析，"
                "以及跨无关行的真隔离事务。kevy 接走对外服务的那条路径，让数据库喘口气。",
                "<b>而且单表的服务型读也能搬。</b>用 <code>TABLE.DECLARE</code> 把类型化列、"
                "二级索引和复合 <code>ORDER BY</code> 路径声明一次——或者用 "
                "<code>kevy-sql</code> 直接编译你手上现成的 PG/MySQL schema 文件——"
                "单张表的读路径（索引化的 WHERE、余下的过滤、ORDER BY、翻页、COUNT）"
                "就编译到 kevy 索引上，查询期没有任何计划器。kevy-sql 是构建期的编译器，"
                "不是 SQL 引擎：join 和临时 SQL 被按名拒绝，它们留在 Postgres。"
                "这正是大多数应用真正在用 ORM 的那一部分。",
            ],
        },
        {
            "t": "table",
            "h2": "哪些部分该搬",
            "intro": "按负载逐条来。红色的那三行，是最多人搞错的地方。",
            "head": ["负载", "搬吗", "为什么"],
            "rows": [
                ["会话、令牌", "*搬", "按 key 查表，带一个 TTL。数据库一直是在帮你的忙，那本来就不是它的活。"],
                ["限流、计数器", "*搬", "带过期的 INCR 是原子的，而且 O(1)。在 SQL 里，这是压在你最热那一行上的行锁。"],
                ["任务队列", "*搬", "list 和 stream，带消费组和逐条确认。所谓队列表，不过是一套加了额外步骤的加锁约定。"],
                ["功能开关、配置", "*搬", "一直在读，很少写，从来不 join。"],
                ["单表读（过滤、排序、翻页）", "*搬", "把表的访问路径声明一次——或者用 <code>kevy-sql</code> 编译你的 schema 文件——索引化的 WHERE + ORDER BY + LIMIT 这类读始终是一次查表。见<a href=\"~/use/app-store/\">用索引扛住读</a>。"],
                ["聚合（计数、合计）", "*多数该搬", "物化视图在写入路径上就把它更新好，不必每次读都重算一遍。"],
                ["跨多张表的 join", "!不要搬", "kevy 没有 join，以后也不会长出 join。这正是 Postgres 存在的意义。"],
                ["分析、临时查询", "!不要搬", "这里没有查询计划器，也没有优化器。不要尝试。"],
                ["跨无关行的事务", "!不要搬", "MULTI 是按 shard 生效的，不是全局的。如果你需要跨整个 keyspace 的可串行化隔离，你需要的是数据库。"],
            ],
            "note": (
                "红色那三行不是待办清单，是拒绝——kevy 不会长出 join，也不会长出优化器，"
                "因为把这两样做砸了，比不做还糟。<a href=\"~/docs/rds-workloads/\">每一种关系型"
                "负载，以及它在这里真实的代价</a>，其中也包括那些诚实答案就是留在 Postgres "
                "里的负载。"
            ),
        },
        {
            "t": "code",
            "h2": "怎么从数据库搬过来",
            "caption": "不要一次性切换。一次只搬一种负载，让数据库继续做事实来源，然后测。",
            "text": "# 1. pick ONE workload. sessions are the usual first, because\n#    nothing joins against them and losing one is survivable.\n\n# 2. write to both for a week. reads still come from Postgres.\n#    you are checking that the shapes match, not that it is fast.\n\n# 3. flip reads to kevy. keep the dual write.\nredis-cli SET session:$SID \"$JSON\" EX 3600\n\n# 4. when it has been boring for a fortnight, drop the table.\n\n# then do the next workload. rate limits, then queues, then\n# whichever of your read paths a secondary index can answer.",
        },
        {
            "t": "callout",
            "kind": "note",
            "title": "如果你以后又想走",
            "body": (
                "同样这三条命令，反过来跑一遍就行。<code>kevy-cli export</code> 写出的是一个"
                "普通的 RESP 文件，任何 Redis 兼容的服务端都能导入，而 <code>digest</code> "
                "能证明这份拷贝没有走样。<a href=\"~/docs/migration/\">迁移指南里，"
                "搬出去这件事</a>写得和搬进来一样细——我们宁可你走得干净，"
                "也不希望你是因为被卡住才留下。"
            ),
        },
    ],
}

# ── /choose/ ────────────────────────────────────────────────────────────────

PAGES["choose"] = {
    "title": "你该用 kevy 吗？——kevy",
    "desc": "kevy 的哪一种形态适合你的问题，选它要放弃什么，以及哪些情况下你应该去用别的东西。",
    "foot": "也包括答案是不该用的那些情况",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "选型",
            "h1": "你该用 kevy 吗？",
            "lede": (
                "有时候不该。下面按你真正做决定的顺序来："
                "<b>键值存储这个形状对不对，数据必须待在哪里，以及你要放弃什么。</b>"
            ),
        },
        {
            "t": "prose",
            "h2": "第一步——键值存储这个形状对吗",
            "body": [
                "<b>当你已经知道 key 的时候，用 kevy。</b>一个会话 id、一个用户 id、"
                "一个队列的名字、一个缓存 key。读是查表，不是提问。这覆盖的应用面比大多数人"
                "以为的要大——而且二级索引和物化视图，还能把一部分提问也变成查表。"
                "TABLE 层更进一步：把类型化列和索引声明一次（或者用 <code>kevy-sql</code> "
                "编译一份 PG/MySQL schema 文件），单张表的读路径——索引化的 WHERE、"
                "余下的过滤、ORDER BY、翻页——就始终是一次查表。",
                "<b>当你的读确实是查询的时候，不要用 kevy。</b>跨五张表的 join、"
                "临时的分析查询、跨无关行且要求真隔离的事务——那是 PostgreSQL 的活，"
                "也应该留给 PostgreSQL。我们把<a href=\"~/docs/rds-workloads/\">每一种关系型"
                "负载在这里的代价</a>都写下来了，包括那些答案就是别搬的。",
                "<b>如果一台机器不够用，不要用 kevy。</b>它没有集群模式，以后也不会有。"
                "单个 kevy 每秒能做几百万次操作，而且带上 RAM 预算之后，数据集可以大过 "
                "RAM（冷值下沉到磁盘——RAM 决定键的上限，磁盘决定数据的上限）——"
                "但越过一台机器的吞吐之后，你需要的是一个会分片的东西，那不是这里。",
            ],
        },
        {
            "t": "table",
            "h2": "第二步——数据必须待在哪里",
            "intro": "这一条决定你用哪种形态。每一行里的命令都是同一批。",
            "head": ["你的情况", "用哪种", "为什么"],
            "rows": [
                ["多个服务共用这份数据", "服务端",
                 "一个进程，在一个端口上说 RESP。你现有的 Redis 客户端不用改就能连。"],
                ["数据只属于一个程序", "嵌入",
                 "没有 socket，没有第二个进程，也没有东西要序列化。是一次函数调用，不是一次网络往返。"],
                ["数据属于用户的设备", "浏览器",
                 "481 KB 的 WebAssembly。真的 TTL，真的发布订阅，落在浏览器自己的文件系统上。离线也能用。"],
                ["代码在边缘按请求执行", "边缘",
                 "没有要预热的东西，也不用建连接。存储和你的代码待在同一个 isolate 里。"],
                ["一台没有操作系统、没有堆的设备", "裸机",
                 "kevy-store 是 no_std 的：一块固定 arena，没有分配器。每次 push，CI 都会在 Cortex-M 上把它启动一遍。"],
            ],
            "note": (
                "你不会被锁死。嵌入式 API 和网络协议暴露的是同一组操作，所以一个程序如果长到"
                "进程内的存储装不下，它搬到服务端只需要改打开数据库的方式，"
                "而不必重写使用数据库的代码。"
            ),
        },
        {
            "t": "faq",
            "h2": "第三步——你要放弃什么",
            "items": [
                {
                    "q": "它真的能直接替换 Redis 吗？",
                    "a": "在协议层面，是的——RESP2 和 RESP3，188 条命令，你的客户端库不会察觉。在行为层面，大体上是，而例外恰恰是重点。跨 shard 的 <code>RENAME</code> 不是原子的——多键写只在单个 shard 内原子。另外 SCAN 的游标只在签发它的服务器上有效，与 Redis Cluster 的按节点性质相同。<a href=\"~/docs/commands/\">全部 188 条命令都标着真实的偏差和真实的代价</a>，这些是从实现里读出来的，不是从 Redis 的文档里抄来的。",
                },
                {
                    "q": "数据集必须装进 RAM 吗？",
                    "a": "不再必须。打开分层存储，给 store 一个 RAM 预算：最冷的值下沉到磁盘上一份可丢弃的 value log，访问时换回。冷键上每条命令语义精确不变，append-only 日志的持久化契约不动——RAM 决定你能放多少个键，磁盘决定你能放多少数据。诚实的边界：默认关闭，v1 只下沉字符串和 hash（list、set、sorted set、stream 留在热层），64 字节以下的值从不下沉。<a href=\"~/docs/tiering/\">分层存储指南</a>写明了哪些数字是实测、哪些还是等基准机跑完的目标。",
                },
                {
                    "q": "机器挂了会怎么样？",
                    "a": "每一次写都先落进一份 append-only 日志，启动时重放这份日志。在默认的 <code>everysec</code> fsync 策略下，被硬杀最多丢一秒的写；把 <code>appendfsync = \"always\"</code> 打开就一条都不丢，代价是吞吐。快照存在的唯一目的，是给重放时间设一个上界。<a href=\"~/docs/persistence/\">持久化指南</a>里有具体数字。",
                },
                {
                    "q": "机器故障能扛过去吗？",
                    "a": "能——一主 N 从，带真正的故障切换：计划内的主从交接、带 epoch 围栏的崩溃选举，以及一个可选的一致性阶梯（<code>WAIT</code>、read-your-writes 令牌、有界陈旧度）。你<b>得不到</b>的是把数据分片到多台机器上。副本是拷贝，不是切片。<a href=\"~/docs/availability/\">可用性指南</a>把哪些写能挺过一次故障切换、哪些挺不过，都写明白了。",
                },
                {
                    "q": "有认证吗？",
                    "a": "没有，以后也不会有。没有 AUTH，没有 ACL，没有 TLS——永久不在范围内。把 kevy 跑在内网，或者放在一个真正把这些事做好的代理后面。一层敷衍的认证比坦白没有认证更糟，因为它会引诱人去信任它。",
                },
                {
                    "q": "如果我用得太大了，或者只是改主意了呢？",
                    "a": "<code>kevy-cli export</code> 会把你的 keyspace 写成一个普通的 RESP 文件，任何 Redis 兼容的服务端都能导入它；<code>kevy-cli digest</code> 则在你扔掉任何东西之前，先证明这份拷贝没有走样。<a href=\"~/docs/migration/\">迁移指南</a>里，搬出去写得和搬进来一样细。",
                },
            ],
        },
        {
            "t": "callout",
            "kind": "note",
            "title": "还是拿不准？",
            "body": (
                "打开 <a href=\"~/play/\">playground</a>。那是一个真的 kevy 引擎，"
                "编译成 WebAssembly 跑在你的标签页里——写几个 key，看 TTL 到点消失，"
                "翻一翻躺在你自己磁盘上的 append-only 日志。没有一样是预先录好的，"
                "也没有任何服务端参与。"
            ),
        },
    ],
}

# ── /use/cache/ ─────────────────────────────────────────────────────────────

PAGES["use/cache"] = {
    "title": "缓存与会话——kevy",
    "desc": "在 kevy 里放会话、热点行、限流和功能开关：任务是什么、用哪几条命令、每一步的代价是什么。",
    "foot": "几乎所有人第一个搬过来的负载",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "缓存与会话",
            "h1": "把你数据库最讨厌的那部分负载接走",
            "lede": (
                "会话、限流、功能开关，还有每个请求都要读的那一行热点行。在大多数应用里，"
                "它们都住在 Postgres 里，也正是被敲得最狠的那些行——不是因为数据库做不好，"
                "而是因为<b>它们从来就不是提问。key 你早就知道了。</b>"
            ),
        },
        {
            "t": "prose",
            "h2": "为什么合适",
            "body": [
                "这些东西的形状是同一个：一个你手里已经有的 key、一个很小的 value、"
                "一段寿命。kevy 用 O(1) 查到它，不靠定时任务就让它自己过期，"
                "并且在一台机器上每秒做几百万次。",
                "被低估的那一部分是<b>过期</b>。建在数据库上的缓存需要一个清理任务，"
                "而 bug 就住在清理任务里。在这里，时间一到引擎就把 key 丢掉，"
                "不管有没有人来问它。下面是四个任务——每一个都可以原样粘进 "
                "<code>redis-cli</code>，对着一个跑起来的 kevy 直接执行。",
                "<b>而且长尾可以大过 RAM。</b>开着 RAM 预算（<code>[tiering]</code>）时，"
                "最冷的值会下沉到一份可丢弃的磁盘日志、访问时换回——冷键上每条命令不变，"
                "持久化不动——于是很少被读的会话和归档不再占 RAM，也不用再养第二个存储。"
                "默认关闭；v1 只下沉字符串和 hash。<a href=\"~/docs/tiering/\">分层存储"
                "指南</a>写着诚实的边界。",
            ],
        },
        {
            "t": "recipe",
            "h2": "一个会自己收尾的会话",
            "goal": "一个会话一个 key，最后一次活动一小时后自己消失——没有清理任务，没有定时任务，也没有一张存过期行的表。",
            "cost_t": "成本与限制",
            "items": [
                {
                    "do": "把会话连同寿命一起写进去",
                    "code": """SET session:7f3a '{"user":"ada","role":"admin"}' EX 3600
-> OK""",
                },
                {
                    "do": "每个请求都来读它",
                    "code": """GET session:7f3a
-> "{\\"user\\":\\"ada\\",\\"role\\":\\"admin\\"}"
TTL session:7f3a
-> (integer) 3599""",
                },
                {
                    "do": "滑动过期：有活动就续期",
                    "note": "续的是钟，不是值——会话死在用户安静下来一小时之后，而不是登录一小时之后。",
                    "code": """EXPIRE session:7f3a 3600
-> (integer) 1""",
                },
            ],
            "cost": (
                "一个会话就是一个 key，所以上面每一步都是 O(1) 且原子的。如果你把"
                "一个用户的状态摊在好几个 key 上，原子性到分片为止——在 key 里用 "
                "<code>{hashtag}</code> 把它们放到同一个分片上。"
            ),
        },
        {
            "t": "recipe",
            "h2": "给一个接口限流",
            "goal": "每个客户端每个窗口一个计数器，一分钟内的第 101 个请求会被拒绝。",
            "cost_t": "成本与限制",
            "items": [
                {
                    "do": "把这次请求计进去",
                    "code": """INCR rate:203.0.113.7
-> (integer) 1""",
                },
                {
                    "do": "第一次命中时开窗",
                    "note": "只在返回值是 1 的时候做——后面的请求搭的是已经开着的那个窗口。",
                    "code": """EXPIRE rate:203.0.113.7 60
-> (integer) 1""",
                },
                {
                    "do": "超过上限就拒绝",
                    "note": "计数器越过上限后，由你的 handler 返回 429。窗口继续计数。",
                    "code": """INCR rate:203.0.113.7
-> (integer) 2      (the window survives)""",
                },
            ],
            "cost": (
                "这是一个<b>固定</b>窗口，不是滑动窗口：一阵跨在窗口边界上的突发流量，"
                "短时间内最多能放过两倍的上限。做滥用防护这已经够了，整件事只是两条 "
                "O(1) 命令；要是需要更平滑的整形，多花几个 key，而不是多上一套系统。"
            ),
        },
        {
            "t": "recipe",
            "h2": "功能开关，每个请求都读",
            "goal": "所有开关放在一个 hash 里：热路径上一次 O(1) 的读，一次写就给所有人翻一个开关。",
            "cost_t": "成本与限制",
            "items": [
                {
                    "do": "把开关写进去",
                    "code": """HSET flags new-checkout on dark-mode on beta-search off
-> (integer) 3""",
                },
                {
                    "do": "热路径上读一个",
                    "code": """HGET flags new-checkout
-> "on\"""",
                },
                {
                    "do": "翻一个，立刻对所有人生效",
                    "code": """HSET flags beta-search on
-> (integer) 0      (0 = updated, not added)
HGETALL flags""",
                },
            ],
            "cost": (
                "一个 hash 住在一个分片上，所以每一次开关读都由同一个分片来答。"
                "以开关读的频率，这仍然是每秒几百万次；真有一天它成了热点，"
                "就按业务面或者团队把 hash 拆开。"
            ),
        },
        {
            "t": "recipe",
            "h2": "缓存一行数据库拥有的数据",
            "goal": "热点行从内存里出；Postgres 仍然是事实来源，这份拷贝活不过它的兜底 TTL。",
            "cost_t": "成本与限制",
            "items": [
                {
                    "do": "读未命中时，带着兜底 TTL 回填",
                    "code": """SET user:881 "$json" EX 300
-> OK""",
                },
                {
                    "do": "读走这份拷贝",
                    "code": """GET user:881""",
                },
                {
                    "do": "写的时候让它失效——不要等定时器",
                    "note": "在数据库那边的写提交之后删掉它。下一次读未命中、回填，然后就是对的。",
                    "code": """DEL user:881
-> (integer) 1""",
                },
            ],
            "cost": (
                "缓存是事实的第二份拷贝，它可能是错的——没有任何东西能解决这件事。"
                "<b>要在写的时候让它失效，而不是靠定时器</b>，TTL 是兜底，不是方案本身。"
                "多 key 的 <code>DEL</code> 或 <code>MSET</code> 只在单个分片内是原子的："
                "如果两个 key 必须一起变，用 <code>{hashtag}</code> 把它们放到一起。"
            ),
        },
        {
            "t": "cards",
            "h2": "接下来",
            "intro": "",
            "items": [
                {"kicker": "指南", "title": "食谱", "body": "会话、限流、排行榜、信息流的可用配方。", "go": "去读", "href": "docs/cookbook/"},
                {"kicker": "指南", "title": "持久化", "body": "kill -9 之后什么还在，以及 fsync 策略要你付出什么。", "go": "去读", "href": "docs/persistence/"},
                {"kicker": "参考", "title": "全部命令", "body": "188 条命令，每一条都标着真实代价和相对 Redis 的偏差。", "go": "去查", "href": "docs/commands/"},
            ],
        },
    ],
}

# ── /use/queue/ ─────────────────────────────────────────────────────────────

PAGES["use/queue"] = {
    "title": "队列与后台任务——kevy",
    "desc": "在 kevy 里做任务队列：简单的活用 list，丢不起的活用带消费组的 stream。",
    "foot": "一个不会因为 worker 挂掉就丢任务的队列",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "队列与任务",
            "h1": "把活交给 worker，<br>worker 挂了还能把活拿回来",
            "lede": (
                "关系型数据库里的队列表，本质是一套加了额外步骤的加锁约定。kevy 有两种"
                "真正的队列：丢一个任务还能承受的时候，用 <b>list</b>；承受不了的时候，"
                "用<b>带消费组的 stream</b>。"
            ),
        },
        {
            "t": "prose",
            "h2": "两种选哪种",
            "body": [
                "<b>用 list</b>：任务重做一遍成本很低，worker 也不太可能干到一半挂掉——"
                "发一封邮件、预热一个缓存、让一条 CDN 路径失效。<code>BRPOP</code> "
                "会一直阻塞到有活为止，所以 worker 不必轮询。",
                "<b>用 stream</b>：任务绝对不能丢。消费组会把每条消息恰好交给一个 worker，"
                "并且记住自己交过。如果这个 worker 在确认之前挂了，消息会留在 pending 列表里，"
                "另一个 worker 可以把它认领走——stream 存在的全部理由就是这个，"
                "它也正是一个队列和一份侥幸之间的差别。",
            ],
        },
        {
            "t": "recipe",
            "h2": "list，给可以重来的活",
            "goal": "生产者往里推，阻塞着的 worker 在有活的那一刻醒来。两条命令，没有轮询循环，也没有调度器。",
            "cost_t": "成本与限制",
            "items": [
                {
                    "do": "生产者：把任务推进去",
                    "code": """LPUSH jobs:email '{"to":"ada@example.com","tpl":"welcome"}'
-> (integer) 1""",
                },
                {
                    "do": "worker：阻塞到有活为止",
                    "note": "没有轮询循环，没有 sleep，也没有惊群——任务一到，pop 立刻返回，等满 30 秒还没有就空手返回。",
                    "code": """BRPOP jobs:email 30
-> 1) "jobs:email"
   2) "{\\"to\\":\\"ada@example.com\\",\\"tpl\\":\\"welcome\\"}\"""",
                },
                {
                    "do": "延时任务：score 就是到期时间",
                    "note": "ZPOPMIN.BELOW 是 kevy 自己的命令——它只取真正到期的，遇到第一个没到期的就停下。",
                    "code": """ZADD jobs:due 1783875499 '{"id":"j-91"}'
-> (integer) 1
ZPOPMIN.BELOW jobs:due 1783875500
-> the job payload, only if it is due""",
                },
            ],
            "cost": (
                "<b>被 pop 走却没被 worker 干完的任务，就没了。</b>这是整件事只要"
                "两条命令的代价——只把可以重来的活交给它。另外在多分片的服务端上，"
                "<code>BLPOP</code> 跨多个 key 时并不遵守 Redis 那种严格的从左到右"
                "优先级：连接自己所在分片上的 key 会先被服务。"
            ),
        },
        {
            "t": "recipe",
            "h2": "stream，给丢不起的活",
            "goal": "每个任务恰好交给一个 worker，在被确认之前一直是 pending。挂掉的 worker 手上的任务可以被认领走，全部历史都还在。",
            "cost_t": "成本与限制",
            "items": [
                {
                    "do": "初始化时做一次：建消费组",
                    "code": """XGROUP CREATE jobs:pay g1 $ MKSTREAM
-> OK""",
                },
                {
                    "do": "生产者：把任务追加进去",
                    "code": """XADD jobs:pay * order 4410 amount 8400
-> "1783875499458-0\"""",
                },
                {
                    "do": "worker：先读，再干活，然后确认",
                    "note": "你确认的 ID 就是 XREADGROUP 交到你手上的那个。在 XACK 之前，这个任务一直是 pending——记在你名下，记在账上。",
                    "code": """XREADGROUP GROUP g1 worker-3 COUNT 1 BLOCK 5000 STREAMS jobs:pay >
XACK jobs:pay g1 1783875499458-0""",
                },
                {
                    "do": "worker 在 XACK 之前挂了：把它的任务认领回来",
                    "code": """XAUTOCLAIM jobs:pay g1 worker-7 60000 0-0
# claims anything idle for more than 60 s

XPENDING jobs:pay g1
# what is still outstanding, and who has it""",
                },
            ],
            "cost": (
                "<b>stream 不是免费的。</b>用 <code>MAXLEN</code> 裁剪会重算整条流的权重，"
                "复杂度是整条流的 O(N)——按计划裁剪，不要每次 <code>XADD</code> 都裁。"
                "<code>XREADGROUP</code> 的 <code>COUNT</code> 限制的是交到你手上的条数，"
                "<b>不是扫过的条数</b>：整条尚未投递的尾巴会先被物化出来。"
                "逐条命令的细节在<a href=\"~/docs/commands/\">命令参考</a>里。"
            ),
        },
        {
            "t": "cards",
            "h2": "接下来",
            "intro": "",
            "items": [
                {"kicker": "指南", "title": "食谱", "body": "队列配方，包括重试和死信模式。", "go": "去读", "href": "docs/cookbook/"},
                {"kicker": "参考", "title": "Stream 命令", "body": "XADD、XREADGROUP、XAUTOCLAIM 以及其余命令，都标着真实代价。", "go": "去查", "href": "docs/commands/"},
            ],
        },
    ],
}

# ── /use/realtime/ ──────────────────────────────────────────────────────────

PAGES["use/realtime"] = {
    "title": "实时与发布订阅——kevy",
    "desc": "在 kevy 的发布订阅上做聊天、在线状态、通知和实时仪表盘——也包括一个跟不上的订阅者会怎么样。",
    "foot": "扇出，以及它不保证什么",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "实时",
            "h1": "推给所有<br>正在听的人",
            "lede": (
                "聊天、在线状态、通知、一个会自己更新的仪表盘。一次发布，多个订阅者，"
                "不用轮询。<b>而在浏览器版本里，同一套发布订阅在两个标签页之间也能用，"
                "完全不需要服务端。</b>"
            ),
        },
        {
            "t": "prose",
            "h2": "为什么合适——以及哪里不合适",
            "body": [
                "发布订阅是发完就不管的。一条消息只会送给<b>那一刻</b>正在订阅的人；"
                "晚一秒才连上来的人永远看不到它，而且没有确认。对于一次在线心跳、"
                "一个实时计数器，这正好合适；对于任何你丢了会心疼的东西，这正好不合适。",
                "<b>如果丢一条消息是要紧的，就改用 stream</b>——见"
                "<a href=\"~/use/queue/\">队列</a>。stream 保留历史，支持消费组，"
                "还能让掉过线的客户端把落下的补回来。发布订阅是便宜的那一个；"
                "便宜本身就是这笔交易的内容。",
            ],
        },
        {
            "t": "recipe",
            "h2": "把一条消息扇出给所有正在听的人",
            "goal": "一次发布，送达那一刻在线的每一个订阅者——一个聊天室、一条通知、一个实时计数器。",
            "cost_t": "成本与限制",
            "items": [
                {
                    "do": "每个客户端各自订阅",
                    "note": "PSUBSCRIBE 用一条连接就能订下一整族频道。",
                    "code": """SUBSCRIBE room:42
PSUBSCRIBE room:*          # every room, one connection""",
                },
                {
                    "do": "发布——返回值就是听众数",
                    "code": """PUBLISH room:42 '{"user":"ada","text":"hello"}'
-> (integer) 3             # how many subscribers received it""",
                },
            ],
            "cost": (
                "<b>跟不上的订阅者会被丢掉，而不是被无限缓冲。</b>如果一个客户端跟不上，"
                "它的消息会被丢弃，而不是让服务端的内存无上限地涨下去——这是有意的选择，"
                "在你依赖投递之前应该先知道它。没有确认，也没有重放：只要这两样里有"
                "一样要紧，你要的就是 stream，不是频道。"
                "<a href=\"~/docs/pubsub/\">发布订阅指南</a>把边界写得很具体。"
            ),
        },
        {
            "t": "recipe",
            "h2": "在线状态——现在谁在线",
            "goal": "记账的活交给引擎的过期机制：一个安静下来的客户端，会自己从名单上掉下去。",
            "cost_t": "成本与限制",
            "items": [
                {
                    "do": "心跳：一个带寿命的 key",
                    "note": "客户端每 10 秒续一次。谁停止续期，谁就过期。",
                    "code": """SET presence:ada online EX 30
-> OK""",
                },
                {
                    "do": "名单，用一个 set",
                    "code": """SADD online ada
-> (integer) 1
SMEMBERS online
SREM online ada            # on clean disconnect""",
                },
            ],
            "cost": (
                "靠 TTL 的在线状态是<b>最终</b>正确的：一个崩掉的客户端最长会显示在线"
                "一个 TTL——这个 30 秒要按你能忍多陈旧来定。另外 <code>SMEMBERS</code> "
                "会把整个 set 一次性返回，名单到了百万级，改用 <code>SSCAN</code> 分页。"
            ),
        },
        {
            "t": "recipe",
            "h2": "同一件事，在两个浏览器标签页之间",
            "goal": "同源的两个标签页：在一个里发布，在另一个里渲染。没有服务端，没有 WebSocket，也没有连接状态。",
            "cost_t": "成本与限制",
            "items": [
                {
                    "do": "在每个标签页里打开引擎",
                    "code": """import { open } from "@goliapkg/kevy";

const db = await open({ persist: { name: "app" } });""",
                },
                {
                    "do": "标签页 A 订阅",
                    "code": """db.subscribe("room:42", (payload, channel) => {
  render(JSON.parse(new TextDecoder().decode(payload)));
});""",
                },
                {
                    "do": "标签页 B 发布——A 把它渲染出来",
                    "code": """db.publish("room:42", JSON.stringify({ user: "ada", text: "hello" }));""",
                },
            ],
            "cost": (
                "桥是一个 <code>BroadcastChannel</code>，所以它到达的是<b>同一台设备上"
                "同源的标签页</b>——过滤仍然发生在引擎内部，但要跨设备，那就是服务端的"
                "活了。现在就可以试：在两个标签页里打开 <a href=\"~/play/\">playground</a>，"
                "从任意一边发布。"
            ),
        },
        {
            "t": "cards",
            "h2": "接下来",
            "intro": "",
            "items": [
                {"kicker": "指南", "title": "发布订阅", "body": "频道、模式匹配，以及一个跟不上的订阅者会怎么样。", "go": "去读", "href": "docs/pubsub/"},
                {"kicker": "试一下", "title": "两个标签页，没有服务端", "body": "在两个标签页里打开 playground，从任意一边发布。", "go": "Playground", "href": "#try"},
            ],
        },
    ],
}

# ── /use/ai/ ────────────────────────────────────────────────────────────────

PAGES["use/ai"] = {
    "title": "给 AI 应用的存储——kevy",
    "desc": "向量检索、全文检索和变更流，就在已经存着你的数据的那个存储里。kevy 能给一个 AI 应用什么，又不给什么。",
    "foot": "不带 embedding 模型，这是故意的",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "AI 应用",
            "h1": "数据，和找到数据的方式，<br>同一个存储",
            "lede": (
                "RAG 和 agent 记忆通常意味着三套系统：一个缓存、一个向量数据库、"
                "一个搜索索引——同样的事实存三份，然后慢慢彼此对不上。"
                "<b>kevy 把向量 KNN、BM25 全文检索和变更流都放在引擎里</b>，"
                "直接建在你已经写进去的那些 key 上。"
            ),
        },
        {
            "t": "prose",
            "h2": "为什么合适",
            "body": [
                "RAG 这一套里，贵的不是检索，而是让三份事实保持同步：你写了一篇文档，"
                "然后你还得记住去做 embedding、去建索引、去让缓存失效。"
                "这里每一步都是一个可能忘掉的地方。",
                "<b>在 kevy 里，索引是一句声明，不是一条流水线。</b>你告诉引擎是哪些 key、"
                "哪个字段，写路径就会把索引维持在最新。事后没有东西要跑，"
                "也没有东西会落后。",
                "<b>kevy 不做的那件事，是生成 embedding。</b>引擎里没有模型，以后也不会有"
                "——推理不该待在存储引擎里，硬塞进去只会把你的向量格式绑死在我们的发版节奏上。"
                "向量由你给出；kevy 负责存它、索引它、检索它。",
            ],
        },
        {
            "t": "recipe",
            "h2": "按语义检索你的 key",
            "goal": "在你已经在写的那些 key 的某个字段上做 KNN。声明一次，写路径把它维持在最新，没有东西要同步。",
            "cost_t": "成本与限制",
            "items": [
                {
                    "do": "把索引声明一次",
                    "note": "引擎会回填已有的 key，回填期间回答 INDEXBUILDING。",
                    "code": """IDX.CREATE idx:sem ON PREFIX doc: FIELD vec TYPE vector KIND ann  DIM 768 DISTANCE cosine M 16 EF 200
-> OK""",
                },
                {
                    "do": "照你原来的方式写文档",
                    "code": """HSET doc:4410 title "Ada on pipelining" vec "<768 f32, little-endian>\"""",
                },
                {
                    "do": "最近的十个",
                    "code": """IDX.QUERY idx:sem KNN "<query vector>" LIMIT 10
-> 1) doc:4410
   2) doc:9982""",
                },
            ],
            "cost": (
                "<b>这个索引是 HNSW，是近似的</b>：召回率是一个可调参数（<code>EF</code>），"
                "不是一个保证。第一次构建是对匹配到的 key 做 O(N) 的工作——要提前安排，"
                "不要等它自己撞上来。还有，<b>这里没有 embedding 模型</b>：向量由你给出。"
                "<a href=\"~/docs/vector-search/\">向量检索指南</a>里有那些可以调的参数。"
            ),
        },
        {
            "t": "recipe",
            "h2": "全文检索，以及两种排序融合",
            "goal": "在同一批 key 上做 BM25，再用一条混合查询，把文本排序和向量排序融合在一条命令里。",
            "cost_t": "成本与限制",
            "items": [
                {
                    "do": "在同一批 key 上建文本索引",
                    "code": """IDX.CREATE idx:ft ON PREFIX doc: FIELD title TYPE str KIND text
-> OK""",
                },
                {
                    "do": "匹配，按 BM25 排序",
                    "code": """IDX.QUERY idx:ft MATCH "pipelining"
-> 1) 1) "doc:1"
      2) "0.2877"          # the BM25 score""",
                },
                {
                    "do": "混合：把两种排序融合（RRF）",
                    "code": """IDX.QUERY HYBRID idx:ft MATCH "pipelining" idx:sem KNN "<vector>"  LIMIT 20 RRFK 60""",
                },
            ],
            "cost": (
                "索引的账是在<b>每次写</b>匹配到的 key 时付的——对读多的检索这是对的交易，"
                "对一个每秒重写几千次的 key 是错的。分词（包括中日韩）和 BM25 到哪里为止，"
                "都在<a href=\"~/docs/text-search/\">全文检索指南</a>里。"
            ),
        },
        {
            "t": "recipe",
            "h2": "让 agent 的记忆跟上数据",
            "goal": "从另一个进程持续跟读每一次写——变更时才做 embedding，不靠时刻表，还能从停下的地方续上。",
            "cost_t": "成本与限制",
            "items": [
                {
                    "do": "打开变更流",
                    "code": """# kevy.toml
[feed]
enabled = true""",
                },
                {
                    "do": "找到你的游标",
                    "code": """FEED.SHARDS                 -> (integer) 16
FEED.TAIL 0                 -> 1) (integer) 1     # generation
                               2) (integer) 1     # offset""",
                },
                {
                    "do": "读，处理，续上",
                    "code": """FEED.READ 0 1 0 COUNT 2     -> the writes themselves, replayable""",
                },
            ],
            "cost": (
                "变更流是按分片记的：<code>FEED.SHARDS</code> 告诉你手里有几个游标，"
                "你的消费者按分片各记一个偏移量。它默认是关的——打开 <code>[feed]</code> "
                "才买下写路径上的这份记账。"
                "<a href=\"~/docs/cdc/\">变更流指南</a>讲了怎么跨重启续读。"
            ),
        },
        {
            "t": "callout",
            "kind": "note",
            "title": "如果读这些文档的是一个 agent",
            "body": (
                "<a href=\"~/llms-full.txt\">llms-full.txt</a> 一次抓取就够：每条命令的"
                "真实代价、相对 Redis 的真实偏差，加上每一篇指南的完整正文。"
                "它是从引擎自己的命令表生成的，所以不会和服务端的实际行为脱节。"
            ),
        },
        {
            "t": "cards",
            "h2": "接下来",
            "intro": "",
            "items": [
                {"kicker": "指南", "title": "向量检索", "body": "HNSW、可以调的那些参数，以及近似在这里究竟意味着什么。", "go": "去读", "href": "docs/vector-search/"},
                {"kicker": "指南", "title": "全文检索", "body": "BM25、包含中日韩在内的分词，以及它到哪里为止。", "go": "去读", "href": "docs/text-search/"},
                {"kicker": "指南", "title": "变更流", "body": "从另一个进程持续跟读每一次写，偏移量可以续上。", "go": "去读", "href": "docs/cdc/"},
            ],
        },
    ],
}

# ── /use/app-store/ ─────────────────────────────────────────────────────────

PAGES["use/app-store"] = {
    "title": "不靠数据库扛住读——kevy",
    "desc": "kevy 的二级索引和物化视图：怎样让一个带过滤的列表、一个持续累计的总数，始终是一次查表，而不是变成一次查询。",
    "foot": "大多数应用真正在用 ORM 的那一部分",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "主存储",
            "h1": "让读始终是一次查表",
            "lede": (
                "“这个客户名下所有还没关闭的订单。”“这个购物车里有几件东西。”"
                "这类读，一个应用一秒钟要做上千次，而在关系型数据库里，"
                "每一次都是一条背后跟着查询计划器的查询。<b>kevy 可以直接把答案备好。</b>"
            ),
        },
        {
            "t": "prose",
            "h2": "为什么合适",
            "body": [
                "键值存储被挡在应用数据之外，通常只因为一句反对：<i>可是我需要按 key "
                "以外的东西去查</i>。这句反对是对的，而二级索引正是为它准备的。",
                "<b>索引是声明出来的，不是建出来的。</b>你写清楚 key 的前缀和字段，"
                "写路径会把它维持在最新。一个带过滤的列表于是重新变回一次查表——"
                "没有计划器，没有扫描，没有查询。",
                "<b>视图更进一步</b>，在写入时就把聚合维持在最新，于是一个计数、一个合计是"
                "读出来的，不是算出来的。这正是大多数应用真正在向 ORM 索要的东西，"
                "也正是它们的数据库忙成那样的原因。",
                "<b>而且整张表可以一次声明。</b><code>TABLE.DECLARE</code> 接受类型化列、"
                "二级索引和复合 <code>ORDER BY</code> 路径，在声明期把它们编译成具名索引——"
                "<code>kevy-sql</code> 用你手上现成的 PG/MySQL schema 文件做同一件事。"
                "引擎仍然不做任何规划、不强加任何 schema；join 和运行期 SQL 依旧按名拒绝。",
            ],
        },
        {
            "t": "recipe",
            "h2": "按字段查，而不是按 key 查",
            "goal": "“客户 881 的所有订单”始终是一次查表：每个要查的字段声明一个索引，照常写入，按值来读。",
            "cost_t": "成本与限制",
            "items": [
                {
                    "do": "你的数据，照你本来的方式写",
                    "code": """HSET order:1001 customer 881 status open  total 4400
HSET order:1002 customer 881 status paid  total 8400
HSET order:1003 customer 902 status open  total 1200""",
                },
                {
                    "do": "每个要按它查的字段，建一个索引",
                    "code": """IDX.CREATE idx:cust   ON PREFIX order: FIELD customer TYPE i64 KIND range
IDX.CREATE idx:status ON PREFIX order: FIELD status   TYPE str KIND range""",
                },
                {
                    "do": "那次本来会变成查询的读",
                    "code": """IDX.QUERY idx:cust EQ 881
-> 1) "0"                       # cursor
   2) 1) "order:1001"  2) "881"
      3) "order:1002"  4) "881\"""",
                },
                {
                    "do": "两个条件一起查",
                    "code": """IDX.QUERY COMPOSE AND idx:cust EQ 881 idx:status EQ open
-> 1) "0"
   2) 1) 1) "order:1001\"""",
                },
            ],
            "cost": (
                "<b>索引的账是每次写的时候付的</b>，不是读的时候——对读多的服务路径"
                "这是对的交易，对写多的日志是错的。<b>这里没有 join</b>，以后也不会有："
                "索引回答的是“哪些 key 匹配这些字段”，不是“把这两个集合连起来”。"
                "如果你的读确实需要 join，就把它留在 Postgres 里——"
                "<a href=\"~/docs/rds-workloads/\">关系型负载那一页</a>写着"
                "哪些读属于这种情况。"
            ),
        },
        {
            "t": "recipe",
            "h2": "把一个持续更新的答案备好",
            "goal": "一个带过滤、带排序的列表，由写路径一直维持着——读永远不用重算它，因为它从来没有过期过。",
            "cost_t": "成本与限制",
            "items": [
                {
                    "do": "在同样的索引上声明视图",
                    "note": "括号是各自独立的参数。",
                    "code": """VIEW.CREATE v:open881 QUERY ( AND idx:cust EQ 881 idx:status EQ open )  ORDER BY idx:cust
-> OK""",
                },
                {
                    "do": "读它——这里什么都不用算",
                    "code": """VIEW.QUERY  v:open881
-> 1) "0"
   2) 1) "order:1001"  2) "881\"""",
                },
            ],
            "cost": (
                "视图是<b>写路径上一直要干的活</b>：每一次对匹配 key 的写都会更新它，"
                "不管今天有没有人来读。只为应用真正在对外服务的那些读声明视图，"
                "不再服务的就删掉。视图要组合的那些索引，必须先存在。"
            ),
        },
        {
            "t": "recipe",
            "h2": "整张表，一次声明",
            "goal": "一张关系型表的读路径——索引化的 WHERE、余下的过滤、ORDER BY、翻页、COUNT——由一条声明编译到具名索引上。或者直接由你现成的 schema 文件编译。",
            "cost_t": "成本与限制",
            "items": [
                {
                    "do": "列、索引、排序路径，一条声明写完",
                    "note": "行仍是前缀下的普通 hash——缺列就是 NULL；kevy-cli sql compile schema.sql 会从 CREATE TABLE / CREATE INDEX 生成这一行。",
                    "code": """TABLE.DECLARE orders PREFIX order: PK id COLUMN id str COLUMN customer i64 COLUMN status str COLUMN total f64 INDEX status range VALUES total customer ORDERPATH by_customer ON customer THEN total DESC
-> OK""",
                },
                {
                    "do": "在存储的列上过滤和计数——一行都不用读",
                    "code": """IDX.QUERY orders.status EQ open FILTER total RANGE 2000 inf LIMIT 20
-> 1) "0"
   2) 1) "order:1001"  2) "open"

IDX.COUNT orders.status EQ open
-> (integer) 2""",
                },
                {
                    "do": "ORDER BY customer, total DESC 的那次遍历",
                    "note": "一个复合索引，用关系型复合索引的方式回答它——每个客户的订单，从大到小，不需要重排。",
                    "code": """IDX.QUERY orders.by_customer WHERE customer EQ 881 LIMIT 20 FIELDS status total""",
                },
            ],
            "cost": (
                "<b>没有运行期 SQL，也没有 join。</b>服务端把 <code>SELECT</code> 当作"
                "未知命令拒绝；<code>kevy-cli sql compile</code> 在构建期把 PG/MySQL "
                "schema 文件变成上面这些声明，并按名拒绝 JOIN、子查询和 GROUP BY，"
                "同时指向替代它们的配方。唯一性是校验而非强制，约束是配方而非引擎检查。"
                "开着<a href=\"~/docs/tiering/\">分层存储</a>时，index-only 查询即使"
                "全部行都是冷的也只读 RAM——只有最后的 <code>FIELDS</code> 页会读冷行，"
                "每行一次。"
            ),
        },
        {
            "t": "cards",
            "h2": "接下来",
            "intro": "",
            "items": [
                {"kicker": "指南", "title": "表", "body": "类型化列和索引声明一次，然后像查表一样查它。", "go": "去读", "href": "docs/tables/"},
                {"kicker": "指南", "title": "在 kevy 上做设计", "body": "习惯了用表思考的人，怎么改成用 key 思考。", "go": "去读", "href": "docs/designing-on-kevy/"},
                {"kicker": "指南", "title": "二级索引", "body": "它们怎么建、代价是什么，以及怎么看懂一次查询的执行计划。", "go": "去读", "href": "docs/indexes/"},
                {"kicker": "参考", "title": "关系型负载", "body": "每一种关系型模式，以及在这里做它的真实代价。", "go": "去读", "href": "docs/rds-workloads/"},
            ],
        },
    ],
}

# ── /use/embedded/ ──────────────────────────────────────────────────────────

PAGES["use/embedded"] = {
    "title": "把 kevy 嵌进去——kevy",
    "desc": "把存储放进程序本身：一个桌面应用、一个浏览器标签页、一个边缘 worker，或者一颗没有操作系统的单片机。",
    "foot": "一个引擎，四个地方，没有服务端",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "装进产品里",
            "h1": "把存储<br>放进东西本身",
            "lede": (
                "没有服务端，没有 socket，没有网络。这个引擎可以是一个你直接调用的 struct，"
                "可以是一个 481 KB 的 WebAssembly 模块，也可以是一颗没有操作系统的芯片上的 "
                "no_std 库——<b>而且这三种情况下，它是同一个引擎、同一批命令。</b>"
            ),
        },
        {
            "t": "prose",
            "h2": "为什么合适",
            "body": [
                "每一个必须离线可用的应用，最后都会自己写一层存储。桌面应用会得到 SQLite，"
                "外加一套没人想要的 schema。web 应用会得到 localStorage，然后撞上 5 MB 的"
                "上限，再发现它是同步的、而且只能存字符串，于是又得到 IndexedDB 和包在它"
                "外面的一层抽象。设备会得到一个手写在 flash 里的环形缓冲区。",
                "<b>这三件事是同一个问题，也可以是同一个解。</b>kevy 嵌进去不带任何进程边界，"
                "发到浏览器时带着真的 TTL 和真的发布订阅，也能在一颗 Cortex-M 上用一块固定 "
                "arena、完全不用分配器地启动。最后这一条，CI 每次 push 都会验一遍。",
            ],
        },
        {
            "t": "recipe",
            "h2": "在一个 Rust 程序里",
            "goal": "存储是一个你直接调用的 struct——没有 socket，没有序列化，没有第二个进程。数据是持久的，打开时会重放自己的日志。",
            "cost_t": "成本与限制",
            "items": [
                {
                    "do": "加进来",
                    "code": """# Cargo.toml
kevy-embedded = "4.0\"""",
                },
                {
                    "do": "打开，写，读",
                    "code": """let db = Db::open("data/")?;
db.set(b"session:7f3a", b"{\\"user\\":\\"ada\\"}", Some(Duration::from_secs(3600)))?;
assert_eq!(db.get(b"session:7f3a")?.is_some(), true);""",
                },
                {
                    "do": "以后想用 redis-cli？把 listener 打开",
                    "note": "其他进程通过 RESP 访问同一份存储，上面的代码一行都不用改。",
                    "code": """db.listen("127.0.0.1:6379")?;""",
                },
            ],
            "cost": (
                "<b>嵌入式的存储不是共享的。</b>数据目录归一个进程所有，如果第二个进程"
                "也要这份数据，那正是上面那个 listener——或者"
                "<a href=\"~/docs/embedded-listener/\">完整的服务端</a>——存在的意义。"
                "进程内同样可以给 store 一个 RAM 预算（<code>with_tier_budget</code>）："
                "冷值下沉到磁盘、在你的进程里换回——照实说一句：进程内的一次冷读会在读的"
                "时长内持有 store 的锁，所以可下沉的最大值默认封在 256 KiB。"
                "细节在<a href=\"~/docs/tiering/\">分层存储指南</a>里。"
            ),
        },
        {
            "t": "recipe",
            "h2": "在一个浏览器标签页里",
            "goal": "gzip 之后 481 KB。落在浏览器自己的文件系统上，刷新之后还在，发布订阅还能跨标签页。",
            "cost_t": "成本与限制",
            "items": [
                {
                    "do": "打开它，带持久化",
                    "code": """import { open } from "@goliapkg/kevy";

const db = await open({ persist: { name: "app" } });""",
                },
                {
                    "do": "带着真 TTL 写，刷新之后再读",
                    "code": """db.set("cart:u881", JSON.stringify(items), { ttlMs: 86_400_000 });
db.get("cart:u881");        // still there after a reload
db.pttl("cart:u881");       // the engine expires it, not your code""",
                },
                {
                    "do": "听见别的标签页",
                    "code": """db.subscribe("sync", (payload) => merge(payload));""",
                },
            ],
            "cost": (
                "<b>做一次小的同步读，localStorage 更快</b>——它就是页面自己地址空间里的"
                "一个 map，任何建在 OPFS 上的东西都赢不了它这一点。kevy 赢在那些本来就让 "
                "localStorage 不该被用的地方：真的 TTL、没有 5 MB 上限、value 是字节"
                "而不是字符串、写入不挡主线程。"
            ),
        },
        {
            "t": "recipe",
            "h2": "在一颗单片机上",
            "goal": "no_std，没有分配器，没有操作系统：存储住在一块由你自己定大小的固定 arena 里，CI 每次 push 都会把它启动一遍。",
            "cost_t": "成本与限制",
            "items": [
                {
                    "do": "把它裁到最小",
                    "code": """# Cargo.toml
kevy-store = { version = "4.0", default-features = false }""",
                },
                {
                    "do": "给它内存，然后用",
                    "code": """let mut arena = [0u8; 64 * 1024];
let mut store = Store::new_in(&mut arena);
store.set(b"temp", b"21.4")?;""",
                },
            ],
            "cost": (
                "<b>arena 是固定的。</b>运行时不能再涨——这就是“没有分配器”的含义，"
                "它的大小是你的设计决定，不是引擎的。功能分级和每一级要付多少字节，"
                "都在 <a href=\"~/docs/iot/\">IoT 指南</a>里。"
            ),
        },
        {
            "t": "cards",
            "h2": "接下来",
            "intro": "",
            "items": [
                {"kicker": "指南", "title": "kevy 跑在 WebAssembly 上", "body": "浏览器构建、OPFS 持久化，以及体积预算。", "go": "去读", "href": "docs/wasm/"},
                {"kicker": "指南", "title": "嵌入式 listener", "body": "把引擎嵌进去，同时仍然在 socket 上说 RESP。", "go": "去读", "href": "docs/embedded-listener/"},
                {"kicker": "指南", "title": "IoT 与裸机", "body": "no_std、arena，以及功能分级。", "go": "去读", "href": "docs/iot/"},
            ],
        },
    ],
}

# ── /benchmarks/ ────────────────────────────────────────────────────────────
# Evidence, not a story. Whatever we learned getting the measurement right is in
# bench/ — a reader here wants to know whether the numbers can be trusted and
# where they do not hold, not how we arrived at them.

PAGES["benchmarks"] = {
    "title": "基准测试——kevy",
    "desc": "kevy 4.0 在一台机器上对打 Redis 8、valkey 9.1 和 Dragonfly——也包括 kevy 只是勉强领先的那几条命令。",
    "foot": "可以用仓库里的 bench/ 复现",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "基准测试",
            "h1": "有多快，以及哪里不快",
            "lede": (
                "一台机器，16 核，loopback。每一个数字都能用仓库里的 <code>bench/</code> "
                "复现。<b>做任何决定之前，先把最后两行读完</b>——在那两行上，"
                "性能并不构成换过来的理由。"
            ),
        },
        {
            "t": "table",
            "h2": "四个引擎，一台机器",
            "intro": (
                "50 条连接，小 value。五次运行取中位数，数字取自每个服务端自己的命令计数器，"
                "统计的是三秒稳态窗口内的增量，而不是压测客户端报出来的速率。"
            ),
            "head": ["", "kevy 6.0.0", "Redis 8", "valkey 9.1", "Dragonfly", "vs Redis 8"],
            "rows": [
                ["GET", "7,342,979", "5,835,267", "3,156,107", "2,790,970", "*1.26×"],
                ["SET", "6,695,610", "2,595,547", "1,702,671", "1,870,312", "*2.58×"],
                ["INCR", "6,564,195", "3,315,916", "2,241,966", "2,078,163", "*1.98×"],
                ["SADD", "5,565,412", "3,658,737", "2,209,086", "1,852,751", "*1.52×"],
                ["HSET", "4,388,836", "3,136,470", "1,837,743", "1,759,833", "*1.40×"],
                ["LPUSH", "2,996,776", "2,804,854", "1,876,716", "1,432,854", "!1.07×"],
                ["ZADD", "2,939,276", "2,858,682", "1,760,394", "1,745,073", "!1.03×"],
            ],
            "note": (
                "<b>LPUSH 比 Redis 8 快 12%，ZADD 快 10%。</b>差距只有这么大的时候，"
                "决定胜负的是你的 value 大小和 key 分布，而不是引擎——所以如果 list 或者 "
                "sorted set 是你的热路径，请拿你自己的负载去测，不要为了性能而换。"
                "这两行的颜色是故意标成这样的。"
            ),
        },
        {
            "t": "prose",
            "h2": "这组数字没有告诉你的事",
            "body": [
                "<b>这是 loopback。</b>这里没有网络，而在真实部署里，你等的往往正是网络。"
                "如果你的延迟大部分花在网络上，那么一个 GET 快 2.6× 的引擎，"
                "并不会让你的 p99 也好 2.6×。",
                "<b>value 很小。</b>一个 value 到 64 KB 的时候，整件事的瓶颈会落到内核的 "
                "TCP 路径上，差距收窄到个位数百分比。如果你存的是大块数据，"
                "这些数字说的不是你。",
                "<b>这是一台机器。</b>kevy 没有集群模式。如果你的问题是一台机器不够用，"
                "这一页上没有任何数字帮得上忙。",
            ],
        },
        {
            "t": "table",
            "h2": "浏览器构建产物",
            "intro": "你真正会发到标签页里的东西。",
            "head": ["", "体积", ""],
            "rows": [
                ["kevy.wasm", "1442 KB", "引擎本体，未压缩"],
                ["gzip 之后", "481 KB", "真正过网络的量"],
                ["冷启动", "&lt; 20 ms", "编译加实例化，缓存已热"],
            ],
            "note": (
                "<b>做一次小的同步读，localStorage 比 kevy 快</b>，而且永远会更快——"
                "它就是页面自己地址空间里的一个 map。kevy 赢的是那些本来就让 localStorage "
                "不该被用的地方：真的 TTL、没有 5 MB 上限、value 是字节而不是字符串、"
                "写入不挡主线程。"
            ),
        },
        {
            "t": "code",
            "h2": "自己复现",
            "caption": "两个脚本。这一页上的所有东西都是它们跑出来的。",
            "text": "git clone https://github.com/goliajp/kevy && cd kevy\n\n# four-way: kevy, Redis 8, valkey, Dragonfly\nbash bench/arena.sh\n\n# the regression gate CI runs on every push\nbash bench/perfgate.sh",
        },
    ],
}

PAGES["capacity"] = {
    "title": "容量计算器 — kevy",
    "desc": "固定 RAM 预算 + 分层之下，一个 kevy 进程能服务多少数据：实测公式，交互版。",
    "foot": "公式对实测 RSS 有 ±20% 门禁",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "容量",
            "h1": "装得下多少数据？",
            "lede": (
                "有了<a href=\"~/docs/tiering/\">分层</a>，kevy 能服务比 RAM 大的键空间："
                "热值常驻，冷值下沉到磁盘，每个键留一个常驻存根。天花板由一个实测数字决定"
                "——每条目约 96 B 的地板——本页就是那条公式的交互版。"
                "<b>答案取决于你的值大小</b>；诚实的容量口径写不进一张贴纸。"
            ),
        },
        {
            "t": "calc",
            "h2": "你的数字",
            "intro": (
                "max data:RAM ≈ 值大小 / (96 B + key 堆开销)。≤ 22 B 的 key 内联存放、"
                "不加钱；更长的 key 加自己的字节数。64 B 以下的值永不下沉——存根跟值一样大。"
            ),
            "fields": {
                "value": "典型值大小（字节）",
                "key": "典型 key 大小（字节）",
                "budget": "RAM 预算（GB）",
                "ratio": "data:RAM 上限",
                "served": "该预算能服务的数据量",
                "below": "64 B 以下的值不分层——存根跟值一样大，比值停在 1×。容量就是你的 RAM。",
                "note": "这是模型不是承诺：真实比值落在门禁的 ±20% 带内，下表实测行略低于模型。",
            },
        },
        {
            "t": "table",
            "h2": "实测：同一预算、同一批 key",
            "intro": "只变值大小。完整数据在仓库的 capacity findings。",
            "head": ["值大小", "模型预测", "实测 data:RAM"],
            "rows": [
                ["256 B", "2.67×", "2.65×"],
                ["1 KiB", "10.7×", "10.43×"],
                ["4 KiB", "42.7×", "39.2× —— 全尺度：2 GB 预算服务 80 GB"],
            ],
        },
        {
            "t": "callout",
            "kind": "info",
            "title": "那 96 B 花在哪",
            "body": (
                "是键空间条目本身——内联 key 单元加条目头——每个键都要付，"
                "分不分层都一样；冷存根不占堆。地板和下沉阈值都在 CI 里有门禁"
                "（<code>memgate</code>，±20%），所以这一页不可能悄悄漂离引擎。"
                "细节：<a href=\"~/docs/tiering/\">分层</a>。"
            ),
        },
    ],
}

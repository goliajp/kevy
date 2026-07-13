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
            "eyebrow": "kevy 4.0",
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
                {"label": "打开 Playground", "href": "play/"},
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

# a browser tab — 151 KB, persists to OPFS
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
                "RESP2 和 RESP3，184 条命令——redis-cli 和你的客户端库不用改就能连。"
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
            "us": "kevy 4.0",
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
                    "body": "你今天在用什么，就继续用什么。",
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
                "<b>你的客户端不用改。</b>kevy 说 RESP2 和 RESP3，实现了 184 条命令。"
                "把你现有的库指过来就行，代码不动，redis-cli 不换。没有新的 SDK 要接，"
                "也没有新协议要学。",
                "<b>所以真正要问的只有一句：你能换到什么。</b>只有三样。如果这三样对你都没有"
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
                "<b>而且有一部分提问式的读也能搬。</b>二级索引和物化视图意味着，"
                "一个带过滤条件的列表、一个持续累计的总数，不必变成一次查询——写入路径已经把"
                "答案备好了。这正是大多数应用真正在用 ORM 的那一部分。",
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
                ["带过滤的列表（按状态、按归属）", "*多数该搬", "二级索引不需要查询计划器就能回答。见<a href=\"~/use/app-store/\">用索引扛住读</a>。"],
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
                "以为的要大——而且二级索引和物化视图，还能把一部分提问也变成查表。",
                "<b>当你的读确实是查询的时候，不要用 kevy。</b>跨五张表的 join、"
                "临时的分析查询、跨无关行且要求真隔离的事务——那是 PostgreSQL 的活，"
                "也应该留给 PostgreSQL。我们把<a href=\"~/docs/rds-workloads/\">每一种关系型"
                "负载在这里的代价</a>都写下来了，包括那些答案就是别搬的。",
                "<b>如果一台机器不够用，不要用 kevy。</b>它没有集群模式，以后也不会有。"
                "单个 kevy 每秒能做几百万次操作，这个余量比大多数产品一辈子用到的都多——"
                "但越过它之后，你需要的是一个会分片的东西，那不是这里。",
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
                 "151 KB 的 WebAssembly。真的 TTL，真的发布订阅，落在浏览器自己的文件系统上。离线也能用。"],
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
                    "a": "在协议层面，是的——RESP2 和 RESP3，184 条命令，你的客户端库不会察觉。在行为层面，大体上是，而例外恰恰是重点。跨 shard 的 <code>RENAME</code> 不是原子的——多键写只在单个 shard 内原子。另外 SCAN 的游标只在签发它的服务器上有效，与 Redis Cluster 的按节点性质相同。<a href=\"~/docs/commands/\">全部 184 条命令都标着真实的偏差和真实的代价</a>，这些是从实现里读出来的，不是从 Redis 的文档里抄来的。",
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
    "desc": "在 kevy 里放会话、热点行、限流和功能开关：为什么合适，代价是什么，以及具体用哪些命令。",
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
                "不管有没有人来问它。",
            ],
        },
        {
            "t": "code",
            "h2": "怎么做",
            "caption": "下面每一条命令都是真的。对着一个跑起来的 kevy，用 redis-cli 直接粘贴。",
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
            "title": "代价是什么",
            "body": (
                "缓存是事实的第二份拷贝，它可能是错的。kevy 解决不了这件事——"
                "没有任何东西能解决。<b>要在写的时候让它失效，而不是靠一个定时器</b>，"
                "TTL 是兜底，不是方案本身。另外注意，多 key 的 <code>MSET</code> 或 "
                "<code>DEL</code> 只在单个 shard 内是原子的：如果两个 key 必须一起变，"
                "用 <code>{hashtag}</code> 把它们放到同一个 shard 上。"
            ),
        },
        {
            "t": "cards",
            "h2": "接下来",
            "intro": "",
            "items": [
                {"kicker": "指南", "title": "食谱", "body": "会话、限流、排行榜、信息流的可用配方。", "go": "去读", "href": "docs/cookbook/"},
                {"kicker": "指南", "title": "持久化", "body": "kill -9 之后什么还在，以及 fsync 策略要你付出什么。", "go": "去读", "href": "docs/persistence/"},
                {"kicker": "参考", "title": "全部命令", "body": "184 条命令，每一条都标着真实代价和相对 Redis 的偏差。", "go": "去查", "href": "docs/commands/"},
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
            "t": "code",
            "h2": "怎么做——用 list，做可以重来的活",
            "caption": "worker 直接阻塞。没有轮询循环，没有 sleep，也没有惊群。",
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
            "h2": "怎么做——用 stream，做丢不起的活",
            "caption": "消息在有 worker 确认之前，一直是 pending 的。挂掉的 worker 手上那份活，可以被重新认领。",
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
            "title": "代价是什么",
            "body": (
                "<b>stream 不是免费的。</b>用 <code>MAXLEN</code> 裁剪会重算整条流的权重，"
                "复杂度是整条流的 O(N)——所以要按计划裁剪，不要每次 <code>XADD</code> 都裁。"
                "<code>XREADGROUP</code> 的 <code>COUNT</code> 限制的是交到你手上的条数，"
                "<b>不是扫过的条数</b>：整条尚未投递的尾巴会先被物化出来。另外在多 shard 的"
                "服务端上，<code>BLPOP</code> 跨多个 key 时并不遵守 Redis 那种严格的"
                "从左到右优先级——连接自己所在 shard 上的 key 会先被服务。这些都在"
                "<a href=\"~/docs/commands/\">命令参考</a>里逐条写着。"
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
            "t": "code",
            "h2": "怎么做",
            "caption": "频道，以及一次订阅一整族频道的模式匹配。",
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
            "h2": "同一件事，在浏览器标签页里",
            "caption": "同源的两个标签页，没有服务端，也没有 WebSocket。桥是一个 BroadcastChannel，过滤发生在引擎内部。",
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
            "title": "代价是什么",
            "body": (
                "<b>跟不上的订阅者会被丢掉，而不是被无限缓冲。</b>如果一个客户端跟不上，"
                "它的消息会被丢弃，而不是让服务端的内存无上限地涨下去——这是有意的选择，"
                "在你依赖投递之前，应该先知道它。这里没有确认，也没有重放。"
                "<b>只要这两样里有一样要紧，你要的就是 stream，不是频道。</b>"
                "<a href=\"~/docs/pubsub/\">发布订阅指南</a>把边界写得很具体。"
            ),
        },
        {
            "t": "cards",
            "h2": "接下来",
            "intro": "",
            "items": [
                {"kicker": "指南", "title": "发布订阅", "body": "频道、模式匹配，以及一个跟不上的订阅者会怎么样。", "go": "去读", "href": "docs/pubsub/"},
                {"kicker": "试一下", "title": "两个标签页，没有服务端", "body": "在两个标签页里打开 playground，从任意一边发布。", "go": "Playground", "href": "play/"},
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
                "哪个字段，写入路径就会把索引维持在最新。事后没有东西要跑，"
                "也没有东西会落后。",
                "<b>kevy 不做的那件事，是生成 embedding。</b>引擎里没有模型，以后也不会有"
                "——推理不该待在存储引擎里，硬塞进去只会把你的向量格式绑死在我们的发版节奏上。"
                "向量由你给出；kevy 负责存它、索引它、检索它。",
            ],
        },
        {
            "t": "code",
            "h2": "怎么做——向量检索",
            "caption": "在你的 key 的某个字段上建一个 HNSW 索引。声明一次，之后由写入路径维持最新。",
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
            "h2": "怎么做——全文检索，以及两者合用",
            "caption": "在同一批 key 上做 BM25，用一次混合查询把两种排序融合起来，还有一条可以持续跟读的变更流。",
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
            "title": "代价是什么",
            "body": (
                "<b>建索引是对匹配到的 key 做 O(N) 的工作</b>，在它追平之前，索引会回答 "
                "<code>INDEXBUILDING</code>——第一次建索引要提前安排，不要等它自己撞上来。"
                "<b>向量索引是 HNSW，是近似的</b>：召回率是一个可调参数（<code>EF</code>），"
                "不是一个保证。还有，<b>这里没有 embedding 模型</b>——如果你原本指望 kevy "
                "帮你调一个，它不会，而这件事你应该在做规划之前就知道。"
                "<a href=\"~/docs/vector-search/\">向量检索指南</a>和"
                "<a href=\"~/docs/text-search/\">全文检索指南</a>都写得很具体。"
            ),
        },
        {
            "t": "callout",
            "kind": "note",
            "title": "如果读这些文档的是一个 agent",
            "body": (
                "<a href=\"~/llms-full.txt\">llms-full.txt</a> 一次抓取就够：每条命令的"
                "真实代价、相对 Redis 的真实偏差，加上全部二十四篇指南的完整正文。"
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
                "写入路径会把它维持在最新。一个带过滤的列表于是重新变回一次查表——"
                "没有计划器，没有扫描，没有查询。",
                "<b>视图更进一步</b>，在写入时就把聚合维持在最新，于是一个计数、一个合计是"
                "读出来的，不是算出来的。这正是大多数应用真正在向 ORM 索要的东西，"
                "也正是它们的数据库忙成那样的原因。",
            ],
        },
        {
            "t": "code",
            "h2": "怎么做",
            "caption": "声明索引，照常写入，按字段来读。这里的每一条命令都在一台真实的服务器上跑过。",
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
            "title": "代价是什么",
            "body": (
                "<b>索引和视图的账，是每次写的时候付的</b>，不是读的时候——这就是这笔交易。"
                "对读多的服务路径，它是对的；对写多的日志，它是错的。<b>这里没有 join</b>，"
                "以后也不会有：索引回答的是哪些 key 匹配了这些字段，而不是把两个集合连起来。"
                "如果你的读确实需要 join，就把它留在 Postgres 里。我们把"
                "<a href=\"~/docs/rds-workloads/\">每一种关系型负载在这里的代价</a>"
                "都写下来了，包括那些答案是别搬的。"
            ),
        },
        {
            "t": "cards",
            "h2": "接下来",
            "intro": "",
            "items": [
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
                "可以是一个 151 KB 的 WebAssembly 模块，也可以是一颗没有操作系统的芯片上的 "
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
            "t": "code",
            "h2": "怎么做——在一个 Rust 程序里",
            "caption": "没有 socket，没有序列化，没有第二个进程。数据是持久的，打开时会重放自己的日志。",
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
            "h2": "怎么做——在浏览器标签页里",
            "caption": "gzip 之后 151 KB。落在浏览器自己的文件系统上，刷新之后还在，发布订阅还能跨标签页。",
            "text": """import { open } from "@goliajp/kevy";

const db = await open({ persist: { name: "app" } });

db.set("cart:u881", JSON.stringify(items), { ttlMs: 86_400_000 });
db.get("cart:u881");        // still there after a reload
db.pttl("cart:u881");       // the engine expires it, not your code

db.subscribe("sync", (payload) => merge(payload));   // other tabs""",
        },
        {
            "t": "code",
            "h2": "怎么做——在单片机上",
            "caption": "no_std，没有分配器，没有操作系统。一块由你自己定大小的固定 arena。",
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
            "title": "代价是什么",
            "body": (
                "<b>在浏览器里，做一次小的同步读，localStorage 更快</b>——它就是页面自己"
                "地址空间里的一个 map，任何建在 OPFS 上的东西都赢不了它这一点。kevy 赢在"
                "别的地方，而那些地方本来就是 localStorage 不该被用的理由：真的 TTL、"
                "没有 5 MB 上限、value 是字节而不是字符串、写入不挡主线程。"
                "<b>在单片机上，arena 的大小由你自己定</b>，运行时不能再涨——"
                "这就是没有分配器的含义。还有，<b>嵌入式的存储不是共享的</b>："
                "如果第二个进程也要用这份数据，你要的是服务端，或者嵌入式的 RESP listener。"
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
            "head": ["", "kevy 4.0", "Redis 8", "valkey 9.1", "Dragonfly", "vs Redis 8"],
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
                ["kevy.wasm", "416 KB", "引擎本体，未压缩"],
                ["gzip 之后", "151 KB", "真正过网络的量"],
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

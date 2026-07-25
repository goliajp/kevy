# English content for kevy.golia.jp.
#
# kevy is the data layer for building AI systems. Redis-compatible, faster, and
# it covers what a Redis plus a vector database plus a search index plus a queue
# were covering between them.
#
# The site says exactly two things:
#   1. what the reader needs to see
#   2. what we actually are
#
# It does not talk about our engineering. Not the dependency count, not the
# language, not how carefully we measured — measuring honestly is the floor, not
# an achievement, and a site that congratulates itself for it is a site about
# itself. That material lives in the repository, for the people who go looking.
#
# What DOES belong, because it changes what a reader decides: where we are only
# barely ahead (LPUSH: 12%), what we refuse to do (no cluster, no AUTH, no TLS),
# and which commands do not behave the way Redis's docs say.
#
# Numbers: bench/PERF-LEDGER.md. Sizes: ls -l site/demo/pkg/kevy.wasm.

PAGES = {}

# ── / ───────────────────────────────────────────────────────────────────────

PAGES[""] = {
    "title": "kevy — the data layer for AI systems",
    "desc": "A Redis-compatible data layer for AI systems: same protocol, more throughput, and vector search, full-text, indexes, views and a change feed in one engine. Try it live — the terminal on this page is the real engine, running in your tab.",
    "foot": "GOLIA",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "kevy 4.0",
            "h1": "The data layer<br>for AI systems.",
            "lede": (
                "Redis-compatible — your client connects unchanged. Faster on every "
                "operation. And vector search, full-text, indexes, views and a change "
                "feed live <b>in the engine</b>, not in four services around it. "
                "<b>This terminal is real</b>: the same engine, compiled "
                "to WebAssembly, running in this tab."
            ),
            "ctas": [
                {"label": "cargo install kevy", "href": "#start"},
                {"label": "What it can do", "href": "#code"},
                {"label": "Open the playground", "href": "play/"},
            ],
            "live_term": {
                "hint": "type a command — SET, GET, TTL, INCR, KEYS, SUBSCRIBE, PUBLISH…",
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
            "eyebrow": "What else it does",
            "h2": "One engine. The whole stack an AI system needs.",
            "intro": "Every command below runs against a real server in CI before this page ships. Click through — this is what using kevy looks like.",
            "items": [
                {
                    "label": "Vectors",
                    "code": """# an HNSW index over your keys — declared once,
# kept current by the write path
IDX.CREATE idx:sem ON PREFIX doc: FIELD vec TYPE vector KIND ann DIM 768 DISTANCE cosine M 16 EF 200

HSET doc:4410 title "Ada on pipelining" vec "<768 f32, little-endian>"

# nearest ten. no separate vector database, no sync job.
IDX.QUERY idx:sem KNN "<query vector>" LIMIT 10
-> 1) "doc:4410"
   2) "doc:9982"
""",
                    "note": "You bring the embedding; kevy stores, indexes and searches it. There is no model in the engine, on purpose.",
                    "go": "Agent memory & RAG",
                    "href": "use/ai/",
                },
                {
                    "label": "Full text",
                    "code": """IDX.CREATE idx:ft ON PREFIX doc: FIELD title TYPE str KIND text

IDX.QUERY idx:ft MATCH "pipelining"
-> 1) 1) "doc:1"
      2) "0.2877"          # BM25 score

# hybrid: fuse the text ranking with the vector ranking
IDX.QUERY HYBRID idx:ft MATCH "pipelining" idx:sem KNN "<vector>" LIMIT 20 RRFK 60""",
                    "note": "BM25 with CJK tokenisation, over the same keys the vectors index.",
                    "go": "How search works",
                    "href": "use/ai/",
                },
                {
                    "label": "Indexes",
                    "code": """HSET order:1001 customer 881 status open  total 4400
HSET order:1002 customer 881 status paid  total 8400

IDX.CREATE idx:cust   ON PREFIX order: FIELD customer TYPE i64 KIND range
IDX.CREATE idx:status ON PREFIX order: FIELD status   TYPE str KIND range

# the read that would have been a SQL query
IDX.QUERY COMPOSE AND idx:cust EQ 881 idx:status EQ open
-> 1) "0"
   2) 1) 1) "order:1001"
""",
                    "note": "A filtered read stays a lookup. No query planner, no scan.",
                    "go": "Serving reads without a database",
                    "href": "use/app-store/",
                },
                {
                    "label": "Views",
                    "code": """# the answer, kept current by the WRITE path
VIEW.CREATE v:open881 QUERY ( AND idx:cust EQ 881 idx:status EQ open ) ORDER BY idx:cust

VIEW.QUERY v:open881
-> 1) "0"
   2) 1) "order:1001"  2) "881"

# reads never recompute it; writes keep it fresh""",
                    "note": "What most applications actually want from their ORM.",
                    "go": "Materialised views",
                    "href": "use/app-store/",
                },
                {
                    "label": "Tables",
                    "code": """# a table is a declaration — compiled to named indexes, once
TABLE.DECLARE user PREFIX u: PK id COLUMN id str COLUMN name str COLUMN age i64 COLUMN dept str INDEX age range VALUES dept name ORDERPATH by_dept_age ON dept THEN age DESC

HSET u:1 id 1 name ada age 34 dept eng

# the ORDER BY dept, age DESC walk — one composite index, no planner
IDX.QUERY user.by_dept_age WHERE dept EQ eng LIMIT 20 FIELDS name age""",
                    "note": "Typed columns, secondary indexes, composite ORDER BY paths — even your PG/MySQL schema file, via kevy-cli sql compile. No runtime SQL, no joins: those stay in Postgres.",
                    "go": "Single-table serving",
                    "href": "use/app-store/",
                },
                {
                    "label": "Bigger than RAM",
                    "code": """# kevy.toml — a RAM budget for the whole store
[tiering]
budget = "70%"               # or "4gb", or "auto"

# past the budget, the coldest values spill to a disk log
# and page back on access. a cold key is an ordinary key:
GET archive:2019:q3          # pays one disk read, same reply
TTL archive:2019:q3          # metadata answers from RAM
SCAN 0 MATCH archive:*       # sees cold keys — one key table""",
                    "note": "RAM bounds your keys, disk bounds your data; the AOF durability contract is untouched. v1 spills strings and hashes — lists, sets and streams stay hot.",
                    "go": "How tiering works",
                    "href": "docs/tiering/",
                },
                {
                    "label": "Change feed",
                    "code": """# tail every write from another process — or an agent.
# [feed] enabled = true in kevy.toml
FEED.SHARDS                 -> (integer) 16
FEED.TAIL 0                 -> 1) (integer) 1     # generation
                               2) (integer) 1     # offset
FEED.READ 0 1 0 COUNT 2     -> the writes themselves, replayable""",
                    "note": "Resumable offsets. Nothing to poll, nothing to miss.",
                    "go": "The change feed",
                    "href": "use/ai/",
                },
                {
                    "label": "Anywhere",
                    "code": """# a 16-core server
cargo install kevy && kevy --port 6379

# inside your binary — no socket, no process
let db = Db::open("data/")?;
db.set(b"k", b"v", None)?;

# a browser tab — 218 KB, persists to OPFS
const db = await open({ persist: { name: "app" } });

# a microcontroller — no OS, no allocator
let mut store = Store::new_in(&mut arena);""",
                    "note": "The same engine and the same commands in all four places.",
                    "go": "Embedding kevy",
                    "href": "use/embedded/",
                },
            ],
        },
        {
            "t": "bars",
            "id": "swap",
            "eyebrow": "Why you can replace Redis",
            "h2": "Same protocol. More throughput.",
            "intro": (
                "RESP2 and RESP3, 188 commands — redis-cli and your client library "
                "connect unchanged. One machine, 16 cores, loopback, median of five."
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
            "thin": "under 15% — your workload decides, not the engine",
            "note": (
                "<b>LPUSH and ZADD are only 12% and 10% ahead.</b> If lists or sorted "
                "sets are your hot path, speed is not the reason to switch. "
                "<a href=\"~/benchmarks/\">Full table, against valkey and Dragonfly "
                "too.</a> Migration is three commands — "
                "<a href=\"~/migrate/\">export, import, digest</a> — and works in "
                "both directions."
            ),
        },
        {
            "t": "steps",
            "id": "start",
            "tone": "deep",
            "h2": "Two minutes",
            "intro": "",
            "items": [
                {
                    "title": "Install",
                    "body": "One binary. No runtime, no dependencies to resolve.",
                    "code": "cargo install kevy\nkevy --port 6379",
                },
                {
                    "title": "Point your client at it",
                    "body": (
                        "Whatever you use today keeps working — there is no kevy client "
                        "to install. <b>node-redis</b> / <b>ioredis</b>, <b>go-redis</b>, "
                        "<b>StackExchange.Redis</b>, <b>redis-py</b> and <b>hiredis</b> "
                        "connect unchanged, and kevy's own verbs arrive through the same "
                        "client's raw-command channel. All six run the identical ladder "
                        "against a live server in CI on every push. "
                        "<a href=\"~/docs/clients/\">Examples per language</a>."
                    ),
                    "code": 'redis-cli -p 6379\n> SET greeting hello\nOK\n> TTL greeting\n(integer) -1',
                },
                {
                    "title": "Do something Redis cannot",
                    "body": "Declare an index; the write path keeps it current.",
                    "code": "IDX.CREATE idx:city ON PREFIX user: FIELD city TYPE str KIND range\nIDX.QUERY  idx:city EQ osaka",
                },
            ],
        },
        {
            "t": "callout",
            "kind": "loss",
            "title": "What kevy will not do",
            "body": (
                "<b>It is not a cluster.</b> Replication and failover exist; sharding "
                "across machines does not, and will not. <b>No AUTH, no TLS</b> — run "
                "it on a private network or behind something that does those properly. "
                "<b>Multi-key writes are atomic per shard, not globally</b> — a "
                "cross-shard <code>RENAME</code> or <code>MSET</code> is not one atomic "
                "step. <a href=\"~/docs/commands/\">Every deviation is documented per "
                "command</a>, and <a href=\"~/choose/\">here is when not to use kevy "
                "at all</a>."
            ),
        },
    ],
}

# ── /migrate/ ───────────────────────────────────────────────────────────────

PAGES["migrate"] = {
    "title": "Coming from Redis or a database — kevy",
    "desc": "Why teams move to kevy from Redis or from Postgres, exactly what changes, what it costs, and how to do the move without a rewrite.",
    "foot": "what changes, and what it costs",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "Migrating",
            "h1": "Why you would move,<br>and what it costs you",
            "lede": (
                "Two different conversations. Coming from <b>Redis</b>, the protocol is "
                "the same and the question is what behaves differently. Coming from a "
                "<b>relational database</b>, nothing is the same and the question is "
                "which part of the workload should move at all — the answer is "
                "<b>some of it</b>, and we will say which."
            ),
        },
        {
            "t": "prose",
            "h2": "Coming from Redis",
            "body": [
                "<b>Your client does not change.</b> kevy speaks RESP2 and RESP3 and "
                "answers 188 commands. Point your existing library at it, keep your "
                "code, keep your redis-cli. There is no SDK to adopt and no new "
                "protocol to learn.",
                "<b>So the only real question is what you gain.</b> Four things, and "
                "if none of them is worth anything to you, then stay on Redis — it is "
                "a superb piece of software and switching for its own sake is a waste "
                "of your week.",
            ],
        },
        {
            "t": "steps",
            "h2": "What you gain, concretely",
            "intro": "",
            "items": [
                {
                    "title": "It runs where Redis cannot",
                    "body": "Embed it in a binary, ship it to a browser tab, boot it on a Cortex-M with no allocator. Today each of those needs its own storage layer with its own API; here it is the same engine and the same commands. If you have ever written a second cache for the client side, this is the reason to look.",
                },
                {
                    "title": "It replaces the search service too",
                    "body": "Secondary indexes, materialised views, vector KNN and BM25 full-text are in the engine — not a module, not a sidecar, not a second copy of the data drifting out of sync with the first. Teams running Redis plus a search cluster can often run one thing.",
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
                    "title": "It is faster on the operations you already run",
                    "body": "1.4× on GET, 2.7× on SET, 1.8× on INCR against Redis 8 on the same machine. Read the whole table before you count on it, though — LPUSH and ZADD are only 12% and 10% ahead, and if lists or sorted sets are your hot path this is not the reason to move.",
                },
                {
                    "title": "Your dataset no longer has to fit in RAM",
                    "body": "Give the store a RAM budget and the coldest values spill to a disposable value log on disk, paging back on access — every command unchanged on a cold key, the append-only-log durability contract untouched. RAM bounds how many keys you hold; disk bounds how much data. That replaces the Redis-plus-separate-disk-store split for big-value and long-tail workloads. The honest limits: it is off by default, v1 spills strings and hashes (lists, sets and streams stay hot), and values under 64 bytes never spill — a stub would be as big as the value.",
                    "code": "# kevy.toml\n[tiering]\nbudget = \"70%\"      # or \"4gb\", or \"auto\"",
                },
            ],
        },
        {
            "t": "callout",
            "kind": "loss",
            "title": "What you give up by leaving Redis",
            "body": (
                "<b>No cluster.</b> Replicas are copies, not shards. <b>No AUTH, no "
                "TLS.</b> And <b>a handful of commands behave differently</b> — the one to know: a "
                "cross-shard <code>RENAME</code> is not atomic (multi-key writes are "
                "atomic per shard, not globally). None of these is a bug "
                "and all of them are documented per command. Read the list before you "
                "commit, not after: <a href=\"~/docs/commands/\">every command's real "
                "cost and real deviation</a>."
            ),
        },
        {
            "t": "code",
            "h2": "How to move, from Redis",
            "caption": "Export from Redis, import into kevy, and check the two agree. Every command below was run.",
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
            "h2": "Coming from a relational database",
            "body": [
                "<b>Do not move your database.</b> Move the part of it that was never "
                "a database problem.",
                "Sessions. Rate limits. Feature flags. Job queues. The hot row every "
                "request reads and nobody ever joins against. These live in Postgres "
                "in most applications, and they are the rows getting hammered — not "
                "because a relational database is bad at them, but because they were "
                "never questions. They are lookups. You already know the key.",
                "<b>Keep Postgres for what it is unmatched at</b> — joins, ad-hoc "
                "queries, analytics, transactions with real isolation across unrelated "
                "rows. kevy takes the serving path and gives the database its evenings "
                "back.",
                "<b>And single-table serving reads can move too.</b> Declare typed "
                "columns, secondary indexes and composite <code>ORDER BY</code> paths "
                "once with <code>TABLE.DECLARE</code> — or compile the PG/MySQL schema "
                "file you already have with <code>kevy-sql</code> — and the read path "
                "of one table (indexed WHERE, residual filters, ORDER BY, pagination, "
                "COUNT) compiles onto kevy indexes, with no planner at query time. "
                "kevy-sql is a build-time compiler, not a SQL engine: joins and ad-hoc "
                "SQL are refused by name, and they stay in Postgres. That is the part "
                "of an ORM most applications actually use.",
            ],
        },
        {
            "t": "table",
            "h2": "Which parts should move",
            "intro": "Per workload. The three rows in red are the ones people get wrong.",
            "head": ["Workload", "Move it?", "Why"],
            "rows": [
                ["Sessions, tokens", "*Yes", "A lookup by key with a TTL. The database was doing you a favour, not a job."],
                ["Rate limits, counters", "*Yes", "INCR with an expiry is atomic and O(1). In SQL this is a row lock on your hottest row."],
                ["Job queues", "*Yes", "Lists and streams, with consumer groups and per-message acknowledgement. A queue table is a lock convention with extra steps."],
                ["Feature flags, config", "*Yes", "Read constantly, written rarely, joined never."],
                ["Single-table reads (filtered, ordered, paged)", "*Yes", "Declare the table's access paths once — or compile your schema file with <code>kevy-sql</code> — and the indexed WHERE + ORDER BY + LIMIT read stays a lookup. See <a href=\"~/use/app-store/\">serving reads</a>."],
                ["Aggregates (counts, totals)", "*Often", "A materialised view keeps it current on the write path instead of recomputing it on every read."],
                ["Joins across several tables", "!No", "kevy has no joins and will not grow them. This is what Postgres is for."],
                ["Analytics, ad-hoc queries", "!No", "There is no query planner and no optimiser. Do not try."],
                ["Transactions across unrelated rows", "!No", "MULTI is per shard, not global. If you need serialisable isolation across the keyspace, you need a database."],
            ],
            "note": (
                "The three red rows are not a to-do list. They are refusals — kevy will "
                "not grow joins or an optimiser, because doing either badly is worse "
                "than not doing it. <a href=\"~/docs/rds-workloads/\">Every relational "
                "workload, with what it actually costs here</a>, including the ones "
                "where the honest answer is \"keep it in Postgres\"."
            ),
        },
        {
            "t": "code",
            "h2": "How to move, from a database",
            "caption": "Nothing is cut over at once. Move one workload, keep the database as the source of truth, and measure.",
            "text": "# 1. pick ONE workload. sessions are the usual first, because\n#    nothing joins against them and losing one is survivable.\n\n# 2. write to both for a week. reads still come from Postgres.\n#    you are checking that the shapes match, not that it is fast.\n\n# 3. flip reads to kevy. keep the dual write.\nredis-cli SET session:$SID \"$JSON\" EX 3600\n\n# 4. when it has been boring for a fortnight, drop the table.\n\n# then do the next workload. rate limits, then queues, then\n# whichever of your read paths a secondary index can answer.",
        },
        {
            "t": "callout",
            "kind": "note",
            "title": "And if you want to leave again",
            "body": (
                "The same three commands run in the other direction. "
                "<code>kevy-cli export</code> writes a plain RESP file that any "
                "Redis-compatible server will import, and <code>digest</code> proves "
                "the copy is faithful. <a href=\"~/docs/migration/\">The migration "
                "guide covers moving out</a> as carefully as moving in — we would much "
                "rather you leave cleanly than stay because you are stuck."
            ),
        },
    ],
}

# ── /choose/ ────────────────────────────────────────────────────────────────

PAGES["choose"] = {
    "title": "Should you use kevy? — kevy",
    "desc": "Which face of kevy fits your problem, what you give up by choosing it, and the cases where you should use something else instead.",
    "foot": "including the cases where the answer is no",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "Choosing",
            "h1": "Should you use kevy?",
            "lede": (
                "Sometimes not. This is the decision in the order you actually make "
                "it: <b>is a key-value store even the right shape, where does the data "
                "have to live, and what are you giving up?</b>"
            ),
        },
        {
            "t": "prose",
            "h2": "First — is a key-value store the right shape?",
            "body": [
                "<b>Use kevy when you know the key.</b> A session id, a user id, a "
                "queue's name, a cache key. Reads are lookups, not questions. That "
                "covers more of an application than people expect — and secondary "
                "indexes and materialised views turn some of the questions into "
                "lookups too. The TABLE layer goes further: declare typed columns and "
                "indexes once (or compile a PG/MySQL schema file with "
                "<code>kevy-sql</code>) and a single table's read path — indexed "
                "WHERE, residual filters, ORDER BY, pagination — stays a lookup.",
                "<b>Do not use kevy when your reads are genuinely queries.</b> Joins "
                "across five tables, ad-hoc analytics, a transaction spanning "
                "unrelated rows with real isolation — that is PostgreSQL, and it "
                "should stay PostgreSQL. We wrote down "
                "<a href=\"~/docs/rds-workloads/\">what each relational workload costs "
                "here</a>, including the ones where the answer is do not.",
                "<b>Do not use kevy if one machine is not enough.</b> There is no "
                "cluster mode and there will not be one. A single kevy does several "
                "million operations a second, and with a RAM budget its dataset can "
                "be bigger than RAM (cold values spill to disk — RAM bounds keys, "
                "disk bounds data) — but past one machine's throughput you need "
                "something that shards, and that is not this.",
            ],
        },
        {
            "t": "table",
            "h2": "Second — where does the data have to live?",
            "intro": "This is what picks the shape. The commands are identical in every row.",
            "head": ["Your situation", "Use", "Why"],
            "rows": [
                ["Several services share the data", "Server",
                 "One process, RESP on a port. Your existing Redis client connects unchanged."],
                ["One program owns the data", "Embedded",
                 "No socket, no second process, nothing to serialise. A function call, not a round trip."],
                ["The data belongs to the user's device", "Browser",
                 "218 KB of WebAssembly. Real TTLs and pub/sub, persisted to the browser's filesystem. Works offline."],
                ["Code runs at the edge, per request", "Edge",
                 "Nothing to warm up, no connection to open. The store is in the isolate with your code."],
                ["A device with no OS and no heap", "Bare metal",
                 "kevy-store is no_std: a fixed arena, no allocator. CI boots it on a Cortex-M on every push."],
            ],
            "note": (
                "You are not locked in. The embedded API and the wire protocol expose "
                "the same operations, so a program that outgrows its in-process store "
                "moves to a server by changing how it opens the database — not by "
                "rewriting how it uses it."
            ),
        },
        {
            "t": "faq",
            "h2": "Third — what are you giving up?",
            "items": [
                {
                    "q": "Is it really a drop-in replacement for Redis?",
                    "a": "On the wire, yes — RESP2 and RESP3, 188 commands, and your client library will not notice. In behaviour, mostly, and the exceptions are the point. A cross-shard <code>RENAME</code> is not atomic — multi-key writes are atomic per shard, not globally. And a SCAN cursor is only valid on the server that issued it, the same per-node property Redis Cluster has. <a href=\"~/docs/commands/\">All 188 commands carry their real deviation and their real cost</a>, read out of the implementation rather than copied from Redis's documentation.",
                },
                {
                    "q": "Does the dataset have to fit in RAM?",
                    "a": "Not any more. Turn on tiering and give the store a RAM budget: the coldest values spill to a disposable value log on disk and page back on access. Every command keeps its exact semantics on a cold key, and the append-only-log durability contract is untouched — RAM bounds how many keys you can hold, disk bounds how much data. The honest limits: it is off by default, v1 spills strings and hashes (lists, sets, sorted sets and streams stay hot), and values under 64 bytes never spill. <a href=\"~/docs/tiering/\">The tiering guide</a> states which numbers are measured and which are still targets pending the bench-box run.",
                },
                {
                    "q": "What happens when the machine dies?",
                    "a": "Every write goes to an append-only log first, and the log replays on boot. With the default <code>everysec</code> fsync you lose at most a second of writes to a hard kill; set <code>appendfsync = \"always\"</code> and you lose nothing, at a cost in throughput. Snapshots exist only to bound how long the replay takes. <a href=\"~/docs/persistence/\">The persistence guide</a> has the numbers.",
                },
                {
                    "q": "Can I survive a machine failure?",
                    "a": "Yes — one primary, N replicas, with real failover: planned handover, crash election with epoch fencing, and an opt-in consistency ladder (<code>WAIT</code>, read-your-writes tokens, bounded staleness). What you do <b>not</b> get is data sharded across machines. Replicas are copies, not slices. <a href=\"~/docs/availability/\">The availability guide</a> states exactly which writes survive a failover and which do not.",
                },
                {
                    "q": "Is there authentication?",
                    "a": "No, and there will not be. No AUTH, no ACLs, no TLS — permanently out of scope. Run kevy on a private network, or behind a proxy that does those things properly. A half-hearted auth layer is worse than an honest absence of one, because it invites people to trust it.",
                },
                {
                    "q": "What if I outgrow it, or just change my mind?",
                    "a": "<code>kevy-cli export</code> writes your keyspace to a plain RESP file that any Redis-compatible server will import, and <code>kevy-cli digest</code> proves the copy is faithful before you throw anything away. <a href=\"~/docs/migration/\">The migration guide</a> covers moving out as carefully as moving in.",
                },
            ],
        },
        {
            "t": "callout",
            "kind": "note",
            "title": "Still not sure?",
            "body": (
                "Open the <a href=\"~/play/\">playground</a>. It is a real kevy engine "
                "compiled to WebAssembly, running in your tab — write keys, watch TTLs "
                "expire, look at the append-only log sitting on your own disk. Nothing "
                "is pre-recorded, and no server is involved."
            ),
        },
    ],
}

# ── /use/cache/ ─────────────────────────────────────────────────────────────

PAGES["use/cache"] = {
    "title": "Cache and sessions — kevy",
    "desc": "Sessions, hot rows, rate limits and feature flags in kevy: the task, the commands, and what each one costs.",
    "foot": "the workload almost everyone moves first",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "Cache & sessions",
            "h1": "Take the load your database hates",
            "lede": (
                "Sessions, rate limits, feature flags, the hot row every request "
                "reads. They are in Postgres in most applications, and they are the "
                "rows getting hammered — not because a database is bad at them, but "
                "because <b>they were never questions. You already know the key.</b>"
            ),
        },
        {
            "t": "prose",
            "h2": "Why this fits",
            "body": [
                "Every one of these has the same shape: a key you already hold, a "
                "small value, and a lifetime. kevy does the lookup in O(1), expires "
                "the key itself without a cron job, and does several million of them "
                "a second on one machine.",
                "The part people underrate is the <b>expiry</b>. A cache built on a "
                "database needs a sweeper, and the sweeper is where the bugs live. "
                "Here the engine drops the key when its time is up, whether or not "
                "anyone asks for it. Four tasks below — each one is pasteable into "
                "<code>redis-cli</code> against a running kevy.",
                "<b>And the long tail can outgrow RAM.</b> With a RAM budget on "
                "(<code>[tiering]</code>), the coldest values spill to a disposable "
                "disk log and page back on access — every command unchanged on a cold "
                "key, durability untouched — so rarely-read sessions and archives stop "
                "costing RAM without a second store. Off by default; v1 spills strings "
                "and hashes. <a href=\"~/docs/tiering/\">The tiering guide</a> has the "
                "honest limits.",
            ],
        },
        {
            "t": "recipe",
            "h2": "A session that cleans itself up",
            "goal": "One key per session, gone by itself an hour after the last touch — no sweeper, no cron job, no expired-rows table.",
            "cost_t": "Cost & limits",
            "items": [
                {
                    "do": "Write the session with its lifetime",
                    "code": """SET session:7f3a '{"user":"ada","role":"admin"}' EX 3600
-> OK""",
                },
                {
                    "do": "Read it on every request",
                    "code": """GET session:7f3a
-> "{\\"user\\":\\"ada\\",\\"role\\":\\"admin\\"}"
TTL session:7f3a
-> (integer) 3599""",
                },
                {
                    "do": "Sliding expiry: refresh on activity",
                    "note": "Touch the clock, not the value — the session dies one hour after the user goes quiet, not one hour after login.",
                    "code": """EXPIRE session:7f3a 3600
-> (integer) 1""",
                },
            ],
            "cost": (
                "A session is one key, so every step here is O(1) and atomic. If you "
                "spread one user's state across several keys, atomicity ends at the "
                "shard — co-locate them with a <code>{hashtag}</code> in the key."
            ),
        },
        {
            "t": "recipe",
            "h2": "Rate-limit an endpoint",
            "goal": "One counter per client per window; the 101st request inside a minute gets told no.",
            "cost_t": "Cost & limits",
            "items": [
                {
                    "do": "Count the request",
                    "code": """INCR rate:203.0.113.7
-> (integer) 1""",
                },
                {
                    "do": "Start the window on the first hit",
                    "note": "Only when the reply was 1 — later requests ride the window that already exists.",
                    "code": """EXPIRE rate:203.0.113.7 60
-> (integer) 1""",
                },
                {
                    "do": "Refuse past the limit",
                    "note": "Your handler returns 429 when the counter passes the limit. The window keeps counting.",
                    "code": """INCR rate:203.0.113.7
-> (integer) 2      (the window survives)""",
                },
            ],
            "cost": (
                "This is a <b>fixed</b> window, not a sliding one: a burst straddling "
                "the boundary can pass up to twice the limit in a short stretch. For "
                "abuse control that is fine and the whole thing is two O(1) commands; "
                "if you need smoother shaping, spend more keys, not a new system."
            ),
        },
        {
            "t": "recipe",
            "h2": "Feature flags, read on every request",
            "goal": "All flags in one hash: one O(1) read on the hot path, one write to flip a flag for everyone.",
            "cost_t": "Cost & limits",
            "items": [
                {
                    "do": "Set the flags",
                    "code": """HSET flags new-checkout on dark-mode on beta-search off
-> (integer) 3""",
                },
                {
                    "do": "Read one on the hot path",
                    "code": """HGET flags new-checkout
-> "on\"""",
                },
                {
                    "do": "Flip one, everywhere, now",
                    "code": """HSET flags beta-search on
-> (integer) 0      (0 = updated, not added)
HGETALL flags""",
                },
            ],
            "cost": (
                "One hash lives on one shard, so one shard answers every flag read. "
                "At flag-read rates that is still millions a second; if it ever "
                "becomes the hot spot, split the hash by surface or team."
            ),
        },
        {
            "t": "recipe",
            "h2": "Cache a row your database owns",
            "goal": "The hot row served from memory; Postgres stays the truth and the copy can never outlive its backstop.",
            "cost_t": "Cost & limits",
            "items": [
                {
                    "do": "On a read miss, fill with a backstop TTL",
                    "code": """SET user:881 "$json" EX 300
-> OK""",
                },
                {
                    "do": "Serve reads from the copy",
                    "code": """GET user:881""",
                },
                {
                    "do": "On write, invalidate — do not wait for the timer",
                    "note": "Delete after the database write commits. The next read misses, refills, and is correct.",
                    "code": """DEL user:881
-> (integer) 1""",
                },
            ],
            "cost": (
                "A cache is a second copy of the truth, and it can be wrong — nothing "
                "solves that. <b>Invalidate on write, not on a timer</b>, and keep the "
                "TTL as the backstop rather than the plan. A multi-key <code>DEL</code> "
                "or <code>MSET</code> is atomic only within one shard: if two keys "
                "must change together, co-locate them with a <code>{hashtag}</code>."
            ),
        },
        {
            "t": "cards",
            "h2": "Next",
            "intro": "",
            "items": [
                {"kicker": "Guide", "title": "The cookbook", "body": "Working recipes for sessions, rate limits, leaderboards and feeds.", "go": "Read it", "href": "docs/cookbook/"},
                {"kicker": "Guide", "title": "Persistence", "body": "What survives a kill -9, and what the fsync policy costs you.", "go": "Read it", "href": "docs/persistence/"},
                {"kicker": "Reference", "title": "Every command", "body": "188 commands, each with its real cost and its deviation from Redis.", "go": "Look it up", "href": "docs/commands/"},
            ],
        },
    ],
}

# ── /use/queue/ ─────────────────────────────────────────────────────────────

PAGES["use/queue"] = {
    "title": "Queues and background jobs — kevy",
    "desc": "Job queues in kevy: lists for simple work, streams with consumer groups for work you cannot afford to lose.",
    "foot": "a queue that does not lose a job when a worker dies",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "Queues & jobs",
            "h1": "Hand work to a worker,<br>and get it back if the worker dies",
            "lede": (
                "A queue table in a relational database is a locking convention with "
                "extra steps. kevy has two real queues: <b>a list</b> when losing a "
                "job is survivable, and <b>a stream with consumer groups</b> when it "
                "is not."
            ),
        },
        {
            "t": "prose",
            "h2": "Which of the two",
            "body": [
                "<b>Use a list</b> when the job is cheap to redo and the worker is "
                "unlikely to die mid-task — sending an email, warming a cache, "
                "invalidating a CDN path. <code>BRPOP</code> blocks until there is "
                "work, so the worker does not poll.",
                "<b>Use a stream</b> when the job must not be lost. A consumer group "
                "hands each message to exactly one worker and remembers that it did. "
                "If the worker dies before acknowledging, the message stays in the "
                "pending list and another worker can claim it — that is the whole "
                "reason streams exist, and it is the difference between a queue and a "
                "hope.",
            ],
        },
        {
            "t": "recipe",
            "h2": "A list, for work you can redo",
            "goal": "Producers push; a blocked worker wakes the moment there is work. Two commands, no polling loop, no scheduler.",
            "cost_t": "Cost & limits",
            "items": [
                {
                    "do": "Producer: push the job",
                    "code": """LPUSH jobs:email '{"to":"ada@example.com","tpl":"welcome"}'
-> (integer) 1""",
                },
                {
                    "do": "Worker: block until there is one",
                    "note": "No polling loop, no sleep, no thundering herd — the pop returns the instant a job arrives, or after 30 seconds empty-handed.",
                    "code": """BRPOP jobs:email 30
-> 1) "jobs:email"
   2) "{\\"to\\":\\"ada@example.com\\",\\"tpl\\":\\"welcome\\"}\"""",
                },
                {
                    "do": "Delayed jobs: the score is the due time",
                    "note": "ZPOPMIN.BELOW is kevy's own — it takes only what is actually due and stops at the first job that is not.",
                    "code": """ZADD jobs:due 1783875499 '{"id":"j-91"}'
-> (integer) 1
ZPOPMIN.BELOW jobs:due 1783875500
-> the job payload, only if it is due""",
                },
            ],
            "cost": (
                "<b>A popped job that its worker never finishes is gone.</b> That is "
                "the trade for this being two commands — take it only for work you can "
                "redo. And on a multi-shard server, <code>BLPOP</code> across several "
                "keys does not honour Redis's strict left-to-right priority: keys on "
                "the connection's own shard are served first."
            ),
        },
        {
            "t": "recipe",
            "h2": "A stream, for work you cannot lose",
            "goal": "Each job goes to exactly one worker and stays pending until acknowledged. A dead worker's job is reclaimable, with its full history intact.",
            "cost_t": "Cost & limits",
            "items": [
                {
                    "do": "Once, at setup: create the group",
                    "code": """XGROUP CREATE jobs:pay g1 $ MKSTREAM
-> OK""",
                },
                {
                    "do": "Producer: append the job",
                    "code": """XADD jobs:pay * order 4410 amount 8400
-> "1783875499458-0\"""",
                },
                {
                    "do": "Worker: read, work, then acknowledge",
                    "note": "The ID you acknowledge is the one XREADGROUP handed you. Until the XACK, the job is pending — on you, on the record.",
                    "code": """XREADGROUP GROUP g1 worker-3 COUNT 1 BLOCK 5000 STREAMS jobs:pay >
XACK jobs:pay g1 1783875499458-0""",
                },
                {
                    "do": "A worker died before XACK: reclaim its job",
                    "code": """XAUTOCLAIM jobs:pay g1 worker-7 60000 0-0
# claims anything idle for more than 60 s

XPENDING jobs:pay g1
# what is still outstanding, and who has it""",
                },
            ],
            "cost": (
                "<b>A stream is not free.</b> Trimming with <code>MAXLEN</code> "
                "recomputes the stream's weight, which is O(N) in the whole stream — "
                "trim on a schedule, not on every <code>XADD</code>. And "
                "<code>XREADGROUP</code>'s <code>COUNT</code> bounds what you are "
                "handed, <b>not what is scanned</b>: the entire undelivered tail is "
                "materialised first. Per-command detail is in "
                "<a href=\"~/docs/commands/\">the reference</a>."
            ),
        },
        {
            "t": "cards",
            "h2": "Next",
            "intro": "",
            "items": [
                {"kicker": "Guide", "title": "The cookbook", "body": "Queue recipes, including retry and dead-letter patterns.", "go": "Read it", "href": "docs/cookbook/"},
                {"kicker": "Reference", "title": "Stream commands", "body": "XADD, XREADGROUP, XAUTOCLAIM and the rest, with their real costs.", "go": "Look it up", "href": "docs/commands/"},
            ],
        },
    ],
}

# ── /use/realtime/ ──────────────────────────────────────────────────────────

PAGES["use/realtime"] = {
    "title": "Realtime and pub/sub — kevy",
    "desc": "Chat, presence, notifications and live dashboards on kevy's pub/sub — including what happens to a subscriber that cannot keep up.",
    "foot": "fan-out, and what it does not guarantee",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "Realtime",
            "h1": "Push it to everyone<br>who is listening",
            "lede": (
                "Chat, presence, notifications, a dashboard that updates itself. One "
                "publish, many subscribers, no polling. <b>And in the browser build, "
                "the same pub/sub works between two tabs with no server at all.</b>"
            ),
        },
        {
            "t": "prose",
            "h2": "Why this fits — and where it does not",
            "body": [
                "Pub/sub is fire-and-forget. A message goes to whoever is subscribed "
                "<b>at that moment</b>; nobody who connects a second later will ever "
                "see it, and there is no acknowledgement. That is exactly right for a "
                "presence ping or a live counter, and exactly wrong for anything you "
                "would be upset to lose.",
                "<b>If losing a message matters, use a stream instead</b> — see "
                "<a href=\"~/use/queue/\">queues</a>. Streams keep history, support "
                "consumer groups, and let a client that was offline catch up. Pub/sub "
                "is the cheap thing; the cheapness is the trade.",
            ],
        },
        {
            "t": "recipe",
            "h2": "Fan a message out to everyone listening",
            "goal": "One publish reaches every subscriber connected at that moment — a chat room, a notification, a live counter.",
            "cost_t": "Cost & limits",
            "items": [
                {
                    "do": "Each client subscribes",
                    "note": "PSUBSCRIBE takes a whole family of channels on one connection.",
                    "code": """SUBSCRIBE room:42
PSUBSCRIBE room:*          # every room, one connection""",
                },
                {
                    "do": "Publish — the reply is the audience",
                    "code": """PUBLISH room:42 '{"user":"ada","text":"hello"}'
-> (integer) 3             # how many subscribers received it""",
                },
            ],
            "cost": (
                "<b>A slow subscriber is dropped, not buffered forever.</b> If a "
                "client cannot keep up, its messages are discarded rather than growing "
                "the server's memory without bound — a deliberate choice, and one to "
                "know about before you rely on delivery. No acknowledgement, no "
                "replay: if either matters, you want a stream, not a channel. "
                "<a href=\"~/docs/pubsub/\">The pub/sub guide</a> is specific about "
                "the limits."
            ),
        },
        {
            "t": "recipe",
            "h2": "Presence — who is online right now",
            "goal": "The engine's expiry does the bookkeeping: a client that goes silent falls off the roster by itself.",
            "cost_t": "Cost & limits",
            "items": [
                {
                    "do": "Heartbeat: a key with a lifetime",
                    "note": "The client refreshes every 10 s. Whoever stops refreshing expires.",
                    "code": """SET presence:ada online EX 30
-> OK""",
                },
                {
                    "do": "The roster, as a set",
                    "code": """SADD online ada
-> (integer) 1
SMEMBERS online
SREM online ada            # on clean disconnect""",
                },
            ],
            "cost": (
                "Presence by TTL is <b>eventually</b> right: a crashed client shows "
                "online for up to the TTL — size the 30 s to how stale you can stand. "
                "And <code>SMEMBERS</code> returns the whole set in one reply; on a "
                "roster of millions, page through <code>SSCAN</code> instead."
            ),
        },
        {
            "t": "recipe",
            "h2": "The same thing, between two browser tabs",
            "goal": "Two tabs of the same origin: publish in one, render in the other. No server, no WebSocket, no connection state.",
            "cost_t": "Cost & limits",
            "items": [
                {
                    "do": "Open the engine in each tab",
                    "code": """import { open } from "@goliapkg/kevy";

const db = await open({ persist: { name: "app" } });""",
                },
                {
                    "do": "Tab A subscribes",
                    "code": """db.subscribe("room:42", (payload, channel) => {
  render(JSON.parse(new TextDecoder().decode(payload)));
});""",
                },
                {
                    "do": "Tab B publishes — tab A renders it",
                    "code": """db.publish("room:42", JSON.stringify({ user: "ada", text: "hello" }));""",
                },
            ],
            "cost": (
                "The bridge is a <code>BroadcastChannel</code>, so this reaches "
                "<b>tabs of the same origin on the same device</b> — filtering still "
                "happens inside the engine, but crossing devices is what the server "
                "is for. Try it now: open <a href=\"~/play/\">the playground</a> in "
                "two tabs and publish from either one."
            ),
        },
        {
            "t": "cards",
            "h2": "Next",
            "intro": "",
            "items": [
                {"kicker": "Guide", "title": "Pub/sub", "body": "Channels, patterns, and what happens to a subscriber that falls behind.", "go": "Read it", "href": "docs/pubsub/"},
                {"kicker": "Try it", "title": "Two tabs, no server", "body": "Open the playground in two tabs and publish from either one.", "go": "Playground", "href": "play/"},
            ],
        },
    ],
}

# ── /use/ai/ ────────────────────────────────────────────────────────────────

PAGES["use/ai"] = {
    "title": "Storage for AI applications — kevy",
    "desc": "Vector search, full-text search and a change feed in the store that already holds your data. What kevy gives an AI application, and what it does not.",
    "foot": "no embedding model included, and that is on purpose",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "AI applications",
            "h1": "One store for the data<br>and the way you find it",
            "lede": (
                "RAG and agent memory usually mean three systems: a cache, a vector "
                "database, and a search index — with the same facts in all three, "
                "drifting apart. <b>kevy has vector KNN, BM25 full-text and a change "
                "feed in the engine</b>, over the keys you already wrote."
            ),
        },
        {
            "t": "prose",
            "h2": "Why this fits",
            "body": [
                "The expensive part of a RAG stack is not the search. It is keeping "
                "three copies of the truth in step: you write a document, then you "
                "have to remember to embed it, index it, and invalidate the cache. "
                "Every one of those is a place to forget.",
                "<b>In kevy the index is a declaration, not a pipeline.</b> You tell "
                "the engine which keys and which field, and the write path keeps the "
                "index current. There is nothing to run afterwards and nothing to fall "
                "behind.",
                "<b>What kevy does not do is produce the embedding.</b> There is no "
                "model in the engine and there will not be one — inference does not "
                "belong in a storage engine, and pretending otherwise would tie your "
                "vector format to our release cycle. You bring the vector; kevy stores "
                "it, indexes it and searches it.",
            ],
        },
        {
            "t": "recipe",
            "h2": "Search your keys by meaning",
            "goal": "KNN over a field of the keys you already write. Declared once; the write path keeps it current, with nothing to sync.",
            "cost_t": "Cost & limits",
            "items": [
                {
                    "do": "Declare the index once",
                    "note": "The engine backfills existing keys, answering INDEXBUILDING while it does.",
                    "code": """IDX.CREATE idx:sem ON PREFIX doc: FIELD vec TYPE vector KIND ann  DIM 768 DISTANCE cosine M 16 EF 200
-> OK""",
                },
                {
                    "do": "Write documents the way you already do",
                    "code": """HSET doc:4410 title "Ada on pipelining" vec "<768 f32, little-endian>\"""",
                },
                {
                    "do": "Nearest ten",
                    "code": """IDX.QUERY idx:sem KNN "<query vector>" LIMIT 10
-> 1) doc:4410
   2) doc:9982""",
                },
            ],
            "cost": (
                "<b>The index is HNSW, which is approximate</b>: recall is a tuning "
                "parameter (<code>EF</code>), not a guarantee. The first build is "
                "O(N) over the matching keys — plan it, do not discover it. And "
                "<b>there is no embedding model</b>: you bring the vector. "
                "<a href=\"~/docs/vector-search/\">The vector guide</a> has the "
                "tuning knobs."
            ),
        },
        {
            "t": "recipe",
            "h2": "Full text, and both rankings fused",
            "goal": "BM25 over the same keys, and a hybrid query that fuses the text ranking with the vector ranking in one command.",
            "cost_t": "Cost & limits",
            "items": [
                {
                    "do": "A text index over the same keys",
                    "code": """IDX.CREATE idx:ft ON PREFIX doc: FIELD title TYPE str KIND text
-> OK""",
                },
                {
                    "do": "Match, ranked by BM25",
                    "code": """IDX.QUERY idx:ft MATCH "pipelining"
-> 1) 1) "doc:1"
      2) "0.2877"          # the BM25 score""",
                },
                {
                    "do": "Hybrid: fuse both rankings (RRF)",
                    "code": """IDX.QUERY HYBRID idx:ft MATCH "pipelining" idx:sem KNN "<vector>"  LIMIT 20 RRFK 60""",
                },
            ],
            "cost": (
                "Indexes are paid for <b>on every write</b> to a matching key — the "
                "right trade for read-heavy retrieval, the wrong one for a key you "
                "rewrite thousands of times a second. Tokenisation (including CJK) "
                "and where BM25 stops are in "
                "<a href=\"~/docs/text-search/\">the text guide</a>."
            ),
        },
        {
            "t": "recipe",
            "h2": "Keep an agent's memory in step",
            "goal": "Tail every write from another process — embed on change, not on a schedule, and resume from where you stopped.",
            "cost_t": "Cost & limits",
            "items": [
                {
                    "do": "Enable the feed",
                    "code": """# kevy.toml
[feed]
enabled = true""",
                },
                {
                    "do": "Find your cursors",
                    "code": """FEED.SHARDS                 -> (integer) 16
FEED.TAIL 0                 -> 1) (integer) 1     # generation
                               2) (integer) 1     # offset""",
                },
                {
                    "do": "Read, process, resume",
                    "code": """FEED.READ 0 1 0 COUNT 2     -> the writes themselves, replayable""",
                },
            ],
            "cost": (
                "The feed is per shard: <code>FEED.SHARDS</code> tells you how many "
                "cursors you own, and your consumer tracks one offset per shard. It "
                "is off by default — flipping <code>[feed]</code> on is what buys the "
                "write-path bookkeeping. "
                "<a href=\"~/docs/cdc/\">The change-feed guide</a> covers resuming "
                "across restarts."
            ),
        },
        {
            "t": "callout",
            "kind": "note",
            "title": "If the thing reading these docs is an agent",
            "body": (
                "<a href=\"~/llms-full.txt\">llms-full.txt</a> is one fetch: every "
                "command with its real cost and its real deviation from Redis, plus the "
                "complete text of every guide. It is generated from the "
                "engine's own verb table, so it cannot drift from what the server does."
            ),
        },
        {
            "t": "cards",
            "h2": "Next",
            "intro": "",
            "items": [
                {"kicker": "Guide", "title": "Vector search", "body": "HNSW, the tuning knobs, and what approximate actually means here.", "go": "Read it", "href": "docs/vector-search/"},
                {"kicker": "Guide", "title": "Full-text search", "body": "BM25, tokenisation including CJK, and where it stops.", "go": "Read it", "href": "docs/text-search/"},
                {"kicker": "Guide", "title": "The change feed", "body": "Tail every write from another process, with resumable offsets.", "go": "Read it", "href": "docs/cdc/"},
            ],
        },
    ],
}

# ── /use/app-store/ ─────────────────────────────────────────────────────────

PAGES["use/app-store"] = {
    "title": "Serving reads without a database — kevy",
    "desc": "Secondary indexes and materialised views in kevy: how a filtered list or a running total stays a lookup instead of becoming a query.",
    "foot": "the part of an ORM most applications actually use",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "Primary store",
            "h1": "Keep the read a lookup",
            "lede": (
                "\"All orders for this customer, still open.\" \"How many items in "
                "this cart.\" These are the reads an application does a thousand times "
                "a second, and in a relational database each one is a query with a "
                "planner behind it. <b>kevy can keep the answer ready instead.</b>"
            ),
        },
        {
            "t": "prose",
            "h2": "Why this fits",
            "body": [
                "A key-value store is usually rejected for application data with a "
                "single objection: <i>but I need to look things up by something other "
                "than the key</i>. That objection is correct, and it is what secondary "
                "indexes are for.",
                "<b>An index is declared, not built.</b> You name the key pattern and "
                "the field; the write path keeps it current. A filtered list becomes a "
                "lookup again — no planner, no scan, no query.",
                "<b>A view goes further</b> and keeps an aggregate current on write, "
                "so a count or a total is read rather than computed. This is what most "
                "applications are actually asking their ORM for, and it is why their "
                "database is busy.",
                "<b>And a whole table can be declared at once.</b> "
                "<code>TABLE.DECLARE</code> takes typed columns, secondary indexes "
                "and composite <code>ORDER BY</code> paths and compiles them to named "
                "indexes at declare time — <code>kevy-sql</code> does the same from "
                "the PG/MySQL schema file you already have. The engine still plans "
                "nothing and enforces no schema; joins and runtime SQL stay refused "
                "by name.",
            ],
        },
        {
            "t": "recipe",
            "h2": "Look things up by a field, not the key",
            "goal": "\"All orders for customer 881\" stays a lookup: declare an index per field, write normally, read by value.",
            "cost_t": "Cost & limits",
            "items": [
                {
                    "do": "Your data, written the way you would anyway",
                    "code": """HSET order:1001 customer 881 status open  total 4400
HSET order:1002 customer 881 status paid  total 8400
HSET order:1003 customer 902 status open  total 1200""",
                },
                {
                    "do": "One index per field you look up by",
                    "code": """IDX.CREATE idx:cust   ON PREFIX order: FIELD customer TYPE i64 KIND range
IDX.CREATE idx:status ON PREFIX order: FIELD status   TYPE str KIND range""",
                },
                {
                    "do": "The read that would have been a query",
                    "code": """IDX.QUERY idx:cust EQ 881
-> 1) "0"                       # cursor
   2) 1) "order:1001"  2) "881"
      3) "order:1002"  4) "881\"""",
                },
                {
                    "do": "Two conditions at once",
                    "code": """IDX.QUERY COMPOSE AND idx:cust EQ 881 idx:status EQ open
-> 1) "0"
   2) 1) 1) "order:1001\"""",
                },
            ],
            "cost": (
                "<b>Indexes are paid for on every write</b>, not on read — the right "
                "trade for read-heavy serving, the wrong one for a write-heavy log. "
                "<b>There are no joins</b>, and there will not be: an index answers "
                "\"which keys match these fields\", not \"join these two "
                "collections\". If your read genuinely needs a join, keep it in "
                "Postgres — <a href=\"~/docs/rds-workloads/\">the RDS workloads "
                "page</a> says which ones those are."
            ),
        },
        {
            "t": "recipe",
            "h2": "Keep a running answer ready",
            "goal": "A filtered, ordered list maintained on the write path — the read never recomputes it, because it was never stale.",
            "cost_t": "Cost & limits",
            "items": [
                {
                    "do": "Declare the view over the same indexes",
                    "note": "The parens are separate arguments.",
                    "code": """VIEW.CREATE v:open881 QUERY ( AND idx:cust EQ 881 idx:status EQ open )  ORDER BY idx:cust
-> OK""",
                },
                {
                    "do": "Read it — nothing is computed here",
                    "code": """VIEW.QUERY  v:open881
-> 1) "0"
   2) 1) "order:1001"  2) "881\"""",
                },
            ],
            "cost": (
                "A view is <b>write-path work forever</b>: every write to a matching "
                "key updates it, whether or not anyone reads it today. Declare views "
                "for the reads your application actually serves, and drop the ones it "
                "stops serving. The indexes a view composes must exist first."
            ),
        },
        {
            "t": "recipe",
            "h2": "Declare the whole table once",
            "goal": "The read path of a relational table — indexed WHERE, residual filter, ORDER BY, pagination, COUNT — compiled onto named indexes by one declaration. Or by your existing schema file.",
            "cost_t": "Cost & limits",
            "items": [
                {
                    "do": "Columns, indexes and sort paths, in one declaration",
                    "note": "Rows stay ordinary hashes under the prefix — a missing column is NULL, and kevy-cli sql compile schema.sql emits this line from CREATE TABLE / CREATE INDEX.",
                    "code": """TABLE.DECLARE orders PREFIX order: PK id COLUMN id str COLUMN customer i64 COLUMN status str COLUMN total f64 INDEX status range VALUES total customer ORDERPATH by_customer ON customer THEN total DESC
-> OK""",
                },
                {
                    "do": "Filter and count on stored columns — no row is read",
                    "code": """IDX.QUERY orders.status EQ open FILTER total RANGE 2000 inf LIMIT 20
-> 1) "0"
   2) 1) "order:1001"  2) "open"

IDX.COUNT orders.status EQ open
-> (integer) 2""",
                },
                {
                    "do": "The ORDER BY customer, total DESC walk",
                    "note": "One composite index answers it the way a relational composite index does — each customer's orders, largest first, no re-sort.",
                    "code": """IDX.QUERY orders.by_customer WHERE customer EQ 881 LIMIT 20 FIELDS status total""",
                },
            ],
            "cost": (
                "<b>No runtime SQL and no joins.</b> The server refuses "
                "<code>SELECT</code> as an unknown command; <code>kevy-cli sql "
                "compile</code> turns a PG/MySQL schema file into these declarations "
                "at build time and refuses JOIN, subqueries and GROUP BY by name, "
                "pointing at the recipe that replaces each. Uniqueness is "
                "verify-not-enforce, and constraints are recipes, not engine checks. "
                "With <a href=\"~/docs/tiering/\">tiering</a> on, index-only queries "
                "answer from RAM even when every row is cold — only the final "
                "<code>FIELDS</code> page reads cold rows, one read per row."
            ),
        },
        {
            "t": "cards",
            "h2": "Next",
            "intro": "",
            "items": [
                {"kicker": "Guide", "title": "Tables", "body": "Declare typed columns and indexes once; query them like a table.", "go": "Read it", "href": "docs/tables/"},
                {"kicker": "Guide", "title": "Designing on kevy", "body": "How to think in keys when you are used to thinking in tables.", "go": "Read it", "href": "docs/designing-on-kevy/"},
                {"kicker": "Guide", "title": "Secondary indexes", "body": "How they build, what they cost, and how to explain a query plan.", "go": "Read it", "href": "docs/indexes/"},
                {"kicker": "Reference", "title": "RDS workloads", "body": "Every relational pattern, with the honest cost of doing it here.", "go": "Read it", "href": "docs/rds-workloads/"},
            ],
        },
    ],
}

# ── /use/embedded/ ──────────────────────────────────────────────────────────

PAGES["use/embedded"] = {
    "title": "Embedding kevy — kevy",
    "desc": "Put the store inside the program: a desktop app, a browser tab, an edge worker, or a microcontroller with no operating system.",
    "foot": "one engine, four places, no server",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "Ship it inside",
            "h1": "Put the store<br>inside the thing",
            "lede": (
                "No server, no socket, no network. The engine is a struct you call, a "
                "218 KB WebAssembly module, or a no_std library on a chip with no "
                "operating system — <b>and it is the same engine, with the same "
                "commands, in all three.</b>"
            ),
        },
        {
            "t": "prose",
            "h2": "Why this fits",
            "body": [
                "Every application that has to work offline ends up writing a storage "
                "layer. A desktop app gets SQLite and a schema nobody wanted. A web app "
                "gets localStorage, discovers the 5 MB ceiling and the fact that it is "
                "synchronous and string-only, and then gets IndexedDB and an "
                "abstraction over it. A device gets a hand-rolled ring buffer in flash.",
                "<b>All three are the same problem, and they can be the same "
                "solution.</b> kevy embeds with no process boundary, ships to a browser "
                "with real TTLs and pub/sub, and boots on a Cortex-M with a fixed arena "
                "and no allocator at all. CI proves the last one on every push.",
            ],
        },
        {
            "t": "recipe",
            "h2": "Inside a Rust program",
            "goal": "The store is a struct you call — no socket, no serialisation, no second process. Durable, and it replays its log on open.",
            "cost_t": "Cost & limits",
            "items": [
                {
                    "do": "Add it",
                    "code": """# Cargo.toml
kevy-embedded = "4.0\"""",
                },
                {
                    "do": "Open, write, read",
                    "code": """let db = Db::open("data/")?;
db.set(b"session:7f3a", b"{\\"user\\":\\"ada\\"}", Some(Duration::from_secs(3600)))?;
assert_eq!(db.get(b"session:7f3a")?.is_some(), true);""",
                },
                {
                    "do": "Need redis-cli later? Open the listener",
                    "note": "Other processes reach the same store over RESP, without changing any of the above.",
                    "code": """db.listen("127.0.0.1:6379")?;""",
                },
            ],
            "cost": (
                "<b>An embedded store is not shared.</b> One process owns the data "
                "directory; if a second process needs the data, that is what the "
                "listener above — or <a href=\"~/docs/embedded-listener/\">the full "
                "server</a> — is for. The store can also take a RAM budget in-process "
                "(<code>with_tier_budget</code>): cold values spill to disk and page "
                "back inside your process — with the honest note that an in-process "
                "cold read holds the store's lock for the read's duration, which is "
                "why the largest spillable value is capped at 256 KiB by default. "
                "<a href=\"~/docs/tiering/\">The tiering guide</a> has the details."
            ),
        },
        {
            "t": "recipe",
            "h2": "In a browser tab",
            "goal": "218 KB gzipped. Persists to the browser's own filesystem, survives a reload, and speaks pub/sub across tabs.",
            "cost_t": "Cost & limits",
            "items": [
                {
                    "do": "Open it, persistent",
                    "code": """import { open } from "@goliapkg/kevy";

const db = await open({ persist: { name: "app" } });""",
                },
                {
                    "do": "Write with a real TTL, read after a reload",
                    "code": """db.set("cart:u881", JSON.stringify(items), { ttlMs: 86_400_000 });
db.get("cart:u881");        // still there after a reload
db.pttl("cart:u881");       // the engine expires it, not your code""",
                },
                {
                    "do": "Hear the other tabs",
                    "code": """db.subscribe("sync", (payload) => merge(payload));""",
                },
            ],
            "cost": (
                "<b>localStorage is faster for a small synchronous read</b> — it is a "
                "map in the page's own address space, and nothing built on OPFS will "
                "beat it at that. kevy wins on everything that makes localStorage a "
                "bad idea anyway: real TTLs, no 5 MB ceiling, byte values rather than "
                "strings, and writes that do not block the main thread."
            ),
        },
        {
            "t": "recipe",
            "h2": "On a microcontroller",
            "goal": "no_std, no allocator, no operating system: the store lives in a fixed arena you size yourself, and CI boots it on every push.",
            "cost_t": "Cost & limits",
            "items": [
                {
                    "do": "Strip it down",
                    "code": """# Cargo.toml
kevy-store = { version = "4.0", default-features = false }""",
                },
                {
                    "do": "Give it memory, use it",
                    "code": """let mut arena = [0u8; 64 * 1024];
let mut store = Store::new_in(&mut arena);
store.set(b"temp", b"21.4")?;""",
                },
            ],
            "cost": (
                "<b>The arena is fixed.</b> There is no growing it at runtime — that "
                "is what \"no allocator\" means, and sizing it is your design "
                "decision, not the engine's. The feature tiers and what each one "
                "costs in bytes are in <a href=\"~/docs/iot/\">the IoT guide</a>."
            ),
        },
        {
            "t": "cards",
            "h2": "Next",
            "intro": "",
            "items": [
                {"kicker": "Guide", "title": "kevy on WebAssembly", "body": "The browser build, OPFS persistence, and the size budget.", "go": "Read it", "href": "docs/wasm/"},
                {"kicker": "Guide", "title": "The embedded listener", "body": "Embed the engine and still speak RESP on a socket.", "go": "Read it", "href": "docs/embedded-listener/"},
                {"kicker": "Guide", "title": "IoT and bare metal", "body": "no_std, the arena, and the feature tiers.", "go": "Read it", "href": "docs/iot/"},
            ],
        },
    ],
}

# ── /benchmarks/ ────────────────────────────────────────────────────────────
# Evidence, not a story. Whatever we learned getting the measurement right is in
# bench/ — a reader here wants to know whether the numbers can be trusted and
# where they do not hold, not how we arrived at them.

PAGES["benchmarks"] = {
    "title": "Benchmarks — kevy",
    "desc": "kevy 4.0 against Redis 8, valkey 9.1 and Dragonfly on one machine — including the commands where kevy is barely ahead.",
    "foot": "reproducible from bench/ in the repository",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "Benchmarks",
            "h1": "How fast, and where it isn't",
            "lede": (
                "One machine, 16 cores, loopback. Every figure is reproducible from "
                "<code>bench/</code> in the repository. <b>Read the last two rows "
                "before you decide anything</b> — they are the ones where speed is not "
                "a reason to switch."
            ),
        },
        {
            "t": "table",
            "h2": "Four engines, one machine",
            "intro": (
                "50 connections, small values. Median of five runs, counted from each "
                "server's own command counter over a three-second steady window rather "
                "than from the benchmark client's reported rate."
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
                "<b>LPUSH is 12% ahead of Redis 8, and ZADD 10%.</b> At that margin "
                "your value sizes and key distribution decide the winner, not the "
                "engine — so if lists or sorted sets are your hot path, benchmark your "
                "own workload and do not switch for speed. The rows are coloured that "
                "way on purpose."
            ),
        },
        {
            "t": "prose",
            "h2": "What this does not tell you",
            "body": [
                "<b>It is loopback.</b> There is no network here, and in a real "
                "deployment the network is usually what you are waiting for. An engine "
                "2.6× faster at GET will not make your p99 2.6× better if most of your "
                "latency is the wire.",
                "<b>The values are small.</b> At 64 KB per value the whole thing "
                "becomes bound by the kernel's TCP path and the gap closes to single "
                "digits. If you store large blobs, these numbers are not about you.",
                "<b>It is one machine.</b> kevy has no cluster mode. If your problem "
                "is that a single machine is not enough, no number on this page helps.",
            ],
        },
        {
            "t": "table",
            "h2": "The browser build",
            "intro": "What you actually ship to a tab.",
            "head": ["", "Size", ""],
            "rows": [
                ["kevy.wasm", "416 KB", "the engine, uncompressed"],
                ["gzipped", "218 KB", "what crosses the wire"],
                ["Cold start", "&lt; 20 ms", "compile and instantiate, warm cache"],
            ],
            "note": (
                "<b>localStorage beats kevy on a small synchronous read</b>, and always "
                "will — it is a map in the page's own address space. kevy wins on the "
                "things that make localStorage a bad idea anyway: real TTLs, no 5 MB "
                "ceiling, byte values rather than strings, and writes that do not block "
                "the main thread."
            ),
        },
        {
            "t": "code",
            "h2": "Reproduce it",
            "caption": "Two scripts. Everything on this page comes out of them.",
            "text": "git clone https://github.com/goliajp/kevy && cd kevy\n\n# four-way: kevy, Redis 8, valkey, Dragonfly\nbash bench/arena.sh\n\n# the regression gate CI runs on every push\nbash bench/perfgate.sh",
        },
    ],
}

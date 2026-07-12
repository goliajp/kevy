# English content for the marketing and scenario pages.
#
# Every number here is traceable. The arena table is bench/PERF-LEDGER.md's
# four-way run (median of 5, server-side steady-state counting over a 3-second
# window, because redis-benchmark's own rps figure is quantised to 250 ms and we
# spent a week chasing a regression that turned out to be that timer). The wasm
# size is `ls -l site/demo/pkg/kevy.wasm` and its gzip.
#
# Where we win narrowly, we say so narrowly. LPUSH at 1.12x over Redis 8 gets
# the --loss colour, not a rounded-up 1.1x in bold. A benchmark page that only
# prints its good rows is an advertisement, and the reader can tell.

PAGES = {}

# ── / ───────────────────────────────────────────────────────────────────────

PAGES[""] = {
    "title": "kevy — a Redis-compatible engine in pure Rust",
    "desc": "A Redis-compatible storage engine written in pure Rust with zero third-party dependencies. Runs as a server, embeds in a binary, compiles to WebAssembly, and fits on a microcontroller.",
    "foot": "pure Rust, no third-party dependencies",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "Version 4.0",
            "h1": "A <span class=\"nb\">Redis-compatible</span> engine,<br>written from nothing.",
            "lede": (
                "kevy speaks RESP, answers 184 commands, and has <b>no third-party "
                "dependencies</b> — not a hashmap crate, not a hasher, not an async "
                "runtime. 33 crates, all ours, all Rust. The same engine runs a "
                "16-core server, embeds in a CLI, compiles to 151 KB of WebAssembly, "
                "and boots on a Cortex-M microcontroller with no allocator at all."
            ),
            "ctas": [
                {"label": "Try it in your browser", "href": "play/"},
                {"label": "Read the docs", "href": "docs/"},
                {"label": "See the numbers", "href": "benchmarks/"},
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
            "h2": "One engine, five places to put it",
            "intro": "Pick by where the data has to live — the API is the same in all five.",
            "items": [
                {
                    "kicker": "Server",
                    "title": "A drop-in Redis replacement",
                    "body": "One shard per core, io_uring on Linux, SO_REUSEPORT. Your existing client does not know it changed.",
                    "go": "Run a server",
                    "href": "docs/server/",
                },
                {
                    "kicker": "Embedded",
                    "title": "A store inside your binary",
                    "body": "No socket, no process, no serialisation. Call it like a HashMap that happens to be durable.",
                    "go": "Embed it",
                    "href": "docs/embedded/",
                },
                {
                    "kicker": "Browser",
                    "title": "151 KB of WebAssembly",
                    "body": "A real keyspace with TTLs and pub/sub in the tab, persisted to OPFS. Not a localStorage wrapper.",
                    "go": "Open the playground",
                    "href": "play/",
                },
                {
                    "kicker": "Edge",
                    "title": "Cold-start in a worker",
                    "body": "No runtime to warm up, no connection to open. The store is in the isolate with your code.",
                    "go": "Deploy to an edge",
                    "href": "docs/edge/",
                },
                {
                    "kicker": "Bare metal",
                    "title": "No allocator, no OS",
                    "body": "kevy-store is no_std. It runs on a Cortex-M with a fixed arena and no heap — CI proves it every push.",
                    "go": "See the MCU probe",
                    "href": "docs/embedded/",
                },
                {
                    "kicker": "Agents",
                    "title": "Memory for an LLM",
                    "body": "Vector and full-text indexes, a change feed, and an llms.txt written from the engine's own verb table.",
                    "go": "Read llms.txt",
                    "href": "llms.txt",
                },
            ],
        },
        {
            "t": "table",
            "h2": "Throughput, measured honestly",
            "intro": (
                "One box, 16 cores, loopback. Median of five runs, counted from the "
                "server's own <code>total_commands_processed</code> over a three-second "
                "steady-state window — <b>not</b> from redis-benchmark's rps figure, "
                "which is quantised to 250 ms and will tell you a comfortable lie."
            ),
            "head": ["", "kevy 4.0", "valkey 9.1", "Redis 8", "Dragonfly", "vs Redis 8"],
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
                "Seven for seven against all three, but read the last two rows. "
                "Against Redis 8, LPUSH and ZADD are ahead by 12% and 10% — inside "
                "the range where your hardware, your value sizes and your key "
                "distribution decide the winner, not us. They are printed in the "
                "colour we reserve for margins that thin, because a benchmark page "
                "that shows only its good rows is an advertisement. "
                "<a href=\"benchmarks/\">The full method, and what it cost us to get it right.</a>"
            ),
        },
        {
            "t": "prose",
            "h2": "Zero dependencies is a design constraint, not a boast",
            "body": [
                "The hash map is ours. The hasher is ours. The RESP parser, the "
                "B-tree, the arena allocator, the io_uring bindings, the event loop, "
                "the geohash, the Lua interpreter — all ours, all Rust, all in this "
                "repository. The only C anywhere near kevy is the handful of syscalls "
                "the kernel will not expose any other way, hand-written in "
                "<code>kevy-sys</code> as <code>unsafe extern \"C\"</code>. We do not "
                "link libc's crate.",
                "This is not purity for its own sake. It is what makes the same code "
                "compile to a 16-core server, a no_std microcontroller, and a "
                "WebAssembly module — because there is nothing in the dependency tree "
                "to tell us we cannot. Every crate that assumes an allocator, a "
                "thread, or a clock is a crate that closes one of those doors.",
                "It also means the supply chain is a thing you can read. 33 crates, "
                "one author, one <code>cargo tree</code> that fits on a screen.",
            ],
        },
        {
            "t": "callout",
            "kind": "loss",
            "title": "What kevy is not",
            "body": (
                "It is not a cluster. There is no gossip, no slot migration, no "
                "sentinel — replication and failover exist, sharding across machines "
                "does not, and it is <a href=\"docs/scope/\">refused on purpose</a>. "
                "There is no AUTH and no TLS: put it behind something that does those "
                "properly. Several commands deviate from Redis, some of them "
                "surprisingly — <code>SCAN</code> is not a cursor iterator, "
                "<code>ZRANK</code> is O(N), <code>SPOP</code> is not random. "
                "<a href=\"docs/commands/\">Every one of them is written down</a>, in "
                "the column Redis's own reference does not have."
            ),
        },
        {
            "t": "steps",
            "h2": "Thirty seconds",
            "intro": "Three ways in. Pick the one that matches where your data lives.",
            "items": [
                {
                    "title": "As a server",
                    "body": "Speaks RESP on 6379. Your redis-cli, your client library, your existing code.",
                    "code": "cargo install kevy\nkevy --port 6379",
                },
                {
                    "title": "In your Rust binary",
                    "body": "No socket, no process. The engine is a struct.",
                    "code": 'kevy-embedded = "4.0"\n\nlet db = Db::open("data/")?;\ndb.set(b"k", b"v", None)?;\nassert_eq!(db.get(b"k")?.as_deref(), Some(&b"v"[..]));',
                },
                {
                    "title": "In a browser tab",
                    "body": "151 KB gzipped. Persists to OPFS, survives a reload, and speaks pub/sub across tabs.",
                    "code": 'import { open } from "@goliajp/kevy";\n\nconst db = await open({ persist: { name: "app" } });\ndb.set("cart:u1", JSON.stringify(items), { ttlMs: 3600_000 });',
                },
            ],
        },
    ],
}

# ── /benchmarks/ ────────────────────────────────────────────────────────────

PAGES["benchmarks"] = {
    "title": "Benchmarks — kevy",
    "desc": "kevy 4.0 against valkey 9.1, Redis 8 and Dragonfly on one box. The method, the numbers, and the week we spent discovering our benchmark harness was lying to us.",
    "foot": "every number on this page is reproducible from bench/",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "Benchmarks",
            "h1": "The numbers, and how we got them wrong first",
            "lede": (
                "Every figure here is reproducible from <code>bench/</code> in the "
                "repository. More usefully: here is the mistake that made our earlier "
                "numbers worthless, how we found it, and what we changed so it cannot "
                "happen again."
            ),
        },
        {
            "t": "callout",
            "kind": "warn",
            "title": "Our benchmark harness was lying to us",
            "body": (
                "redis-benchmark reports throughput from a timer that fires every "
                "250 ms (<code>SHOW_THROUGHPUT_INTERVAL</code>). Every rps it prints "
                "is therefore <code>n / (a multiple of 0.25 s)</code> — and at the "
                "rates kevy runs at, a whole run finishes in a handful of ticks. That "
                "is why our old table had GET and SET reporting digit-for-digit "
                "identical figures, which we had shrugged at. It was quantisation. "
                "We chased a phantom 5% regression for a week before reading "
                "<code>redis-benchmark.c</code>. Every number below is counted from "
                "the server's own <code>INFO stats</code> instead, over a three-second "
                "steady-state window, and the harness now rebuilds the reference "
                "commit and interleaves the two so a box that drifts cannot fake a "
                "result either."
            ),
        },
        {
            "t": "table",
            "h2": "Four-way, one box",
            "intro": (
                "16 cores, loopback, 50 connections, 3-byte values. Median of five "
                "runs; the ± is the sample standard deviation across them."
            ),
            "head": ["", "kevy 4.0", "valkey 9.1", "Redis 8", "Dragonfly", "vs valkey", "vs Redis 8", "vs Dragonfly"],
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
                "Seven for seven. But <b>LPUSH is only 12% ahead of Redis 8, and ZADD "
                "10%</b> — margins where your value sizes and key distribution matter "
                "more than the engine does. If those are your hot commands, benchmark "
                "your own workload; do not take ours. The colour is the point: we "
                "reserve it for rows where we are not comfortably ahead, so that a "
                "glance at this table tells you the truth rather than the headline."
            ),
        },
        {
            "t": "prose",
            "h2": "What this benchmark does not tell you",
            "body": [
                "It is one box, on loopback, with small values. That removes the "
                "network, which in a real deployment is usually the thing you are "
                "actually waiting for. A kevy that is 2.6× faster at GET than valkey "
                "will not make your p99 2.6× better if 90% of your latency is the "
                "wire.",
                "Large values change the shape entirely. At 64 KB per value the whole "
                "thing becomes bound by the kernel's TCP path, and the gap closes to "
                "single digits — we have the perf traces and the write-ups in "
                "<code>bench/</code>, including three separate optimisations that "
                "measurably reduced userspace memcpy and did not move throughput at "
                "all, because memcpy was a tax and not the bottleneck.",
                "The engine is single-node. If your problem is that one machine is "
                "not enough, kevy does not solve it, and no benchmark on this page "
                "will change that.",
            ],
        },
        {
            "t": "table",
            "h2": "The browser build",
            "intro": "What you actually ship to a tab.",
            "head": ["", "Size", "Note"],
            "rows": [
                ["kevy.wasm", "416 KB", "the engine, uncompressed"],
                ["gzipped", "151 KB", "what crosses the wire"],
                ["Cold start", "&lt; 20 ms", "compile plus instantiate, warm cache"],
            ],
            "note": (
                "Against localStorage on small reads, localStorage wins — it is a "
                "synchronous map in the browser's own address space and nothing built "
                "on OPFS will beat it at that. kevy wins on everything that makes "
                "localStorage a bad idea anyway: TTLs, a 5 MB ceiling it does not "
                "have, byte values rather than strings, and writes that do not block "
                "the main thread."
            ),
        },
        {
            "t": "code",
            "h2": "Reproduce it",
            "caption": "Everything on this page comes out of these two scripts.",
            "text": "git clone https://github.com/goliajp/kevy && cd kevy\n\n# four-way arena: kevy, valkey, redis 8, dragonfly\nbash bench/arena.sh\n\n# the regression gate: rebuilds the reference commit and interleaves\nbash bench/perfgate.sh",
        },
    ],
}

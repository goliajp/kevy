# Chinese content for the marketing and scenario pages.
#
# Written in Chinese, not translated from en.py. The English is the source
# material and the schema — same pages, same blocks, same rows, same numbers —
# but the sentences are rebuilt, because a page that reads like a translation
# reads like it was not worth the author's time.
#
# Punctuation: Chinese prose takes full-width marks only. 「,」 for a clause,
# 「、」 between list items, 「。」 to end a sentence, and a 破折号 with no space
# on either side of it. ASCII punctuation belongs to code, to <code> spans, and
# to numbers. tools/check_cjk_punct.py refuses the file otherwise, and it has
# caught this exact mistake in this project four times.
#
# The two thin rows keep their colour. LPUSH at 1.12x and ZADD at 1.10x over
# Redis 8 are printed in the --loss colour in Chinese for the same reason they
# are in English: at that margin the reader's own workload decides the winner,
# and a benchmark page that shows only its good rows is an advertisement.

PAGES = {}

# ── / ───────────────────────────────────────────────────────────────────────

PAGES[""] = {
    "title": "kevy——纯 Rust 写的 Redis 兼容引擎",
    "desc": "一个纯 Rust 写成、零第三方依赖的 Redis 兼容存储引擎。可以当服务端跑，可以嵌进你自己的二进制，可以编译成 WebAssembly，也能塞进一颗单片机。",
    "foot": "纯 Rust，零第三方依赖",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "4.0 版本",
            "h1": "一个 Redis 兼容的引擎，<br>从零写起。",
            "lede": (
                "kevy 说的是 RESP 协议，实现了 184 条命令，而且<b>没有任何第三方依赖</b>"
                "——没有 hashmap crate，没有 hasher，没有异步运行时。33 个 crate，"
                "都是我们自己写的，都是 Rust。同一套引擎，可以撑起一台 16 核的服务器，"
                "可以嵌进一个 CLI，可以编译成 151 KB 的 WebAssembly，也可以在一颗"
                "连分配器都没有的 Cortex-M 单片机上启动。"
            ),
            "ctas": [
                {"label": "在浏览器里试一下", "href": "play/"},
                {"label": "读文档", "href": "docs/"},
                {"label": "看实测数字", "href": "benchmarks/"},
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
            "h2": "一套引擎，几种跑法",
            "intro": "按数据必须待在哪里来挑——不管哪一种，API 都是同一套。",
            "items": [
                {
                    "kicker": "服务端",
                    "title": "可以直接换掉 Redis",
                    "body": "一核一个 shard，Linux 上走 io_uring，配合 SO_REUSEPORT。你现有的客户端不会发现东西换过了。",
                    "go": "跑一个服务端",
                    "href": "docs/tuning/",
                },
                {
                    "kicker": "嵌入",
                    "title": "装在你二进制里的存储",
                    "body": "没有 socket，没有进程，没有序列化。当成一个碰巧会落盘的 HashMap 来用就行。",
                    "go": "嵌进去",
                    "href": "docs/embedded-listener/",
                },
                {
                    "kicker": "浏览器",
                    "title": "151 KB 的 WebAssembly",
                    "body": "标签页里跑一个真的 keyspace，有 TTL，有发布订阅，数据落在 OPFS 上。不是给 localStorage 包一层皮。",
                    "go": "打开 Playground",
                    "href": "play/",
                },
                {
                    "kicker": "边缘",
                    "title": "在 worker 里冷启动",
                    "body": "没有要预热的运行时，也不用建连接。存储和你的代码待在同一个 isolate 里。",
                    "go": "部署到边缘",
                    "href": "docs/wasm/",
                },
                {
                    "kicker": "裸机",
                    "title": "没有分配器，没有操作系统",
                    "body": "kevy-store 是 no_std 的。它在 Cortex-M 上跑，只用一块固定大小的 arena，没有堆——每次 push，CI 都会重新验一遍。",
                    "go": "看 MCU 上的验证",
                    "href": "docs/iot/",
                },
                {
                    "kicker": "智能体",
                    "title": "给 LLM 当记忆",
                    "body": "向量索引和全文索引，一条变更流，还有一份直接从引擎自己的命令表生成的 llms.txt。",
                    "go": "读 llms.txt",
                    "href": "llms.txt",
                },
            ],
        },
        {
            "t": "table",
            "h2": "吞吐量，老老实实测出来的",
            "intro": (
                "同一台机器，16 核，走 loopback。五次运行取中位数，数字是从服务端自己的 "
                "<code>total_commands_processed</code> 里数出来的，取三秒稳态窗口内的增量"
                "——<b>不是</b> redis-benchmark 打印的那个 rps。那个数被一个 250 ms 的定时器"
                "量化过，会给你一句听着很舒服的假话。"
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
                "七条命令，三个对手，七战七胜。但请把最后两行读完。对 Redis 8，LPUSH 只领先 "
                "12%，ZADD 只领先 10%——差距薄到这个程度，决定胜负的是你的硬件、你的 value "
                "大小、你的 key 分布，而不是我们。凡是薄成这样的行，我们一律用这个颜色标出来，"
                "因为一个只把自己好看的行印出来的基准页面，那叫广告。"
                "<a href=\"benchmarks/\">完整的测法，以及我们为了把它测对付出过什么。</a>"
            ),
        },
        {
            "t": "prose",
            "h2": "零依赖是一条设计约束，不是一句吹嘘",
            "body": [
                "哈希表是自己写的。hasher 是自己写的。RESP 解析器、B 树、arena 分配器、"
                "io_uring 绑定、事件循环、geohash、Lua 解释器——全都是自己写的，全都是 Rust，"
                "全都在这个仓库里。整个 kevy 附近唯一的 C，是内核不肯用别的方式暴露出来的"
                "那几个系统调用，在 <code>kevy-sys</code> 里手写成 <code>unsafe extern \"C\"</code>。"
                "libc 的那个 crate，我们不链接。",
                "这不是洁癖。同一份代码之所以能同时编译成一台 16 核的服务端、一块 no_std 的"
                "单片机固件、一个 WebAssembly 模块，靠的正是这条约束——依赖树里没有任何东西"
                "能跳出来告诉我们这做不到。一个 crate 只要假定了分配器、假定了线程、"
                "假定了时钟，它就替你关上其中一扇门。",
                "还有一个结果：供应链是可以读完的。33 个 crate，一个作者，一棵 "
                "<code>cargo tree</code> 一屏放得下。",
            ],
        },
        {
            "t": "callout",
            "kind": "loss",
            "title": "kevy 不是什么",
            "body": (
                "它不是集群。没有 gossip，没有 slot 迁移，没有 sentinel——复制和故障切换有，"
                "跨机器分片没有，而且是<a href=\"docs/cluster/\">故意不做</a>。没有 AUTH，"
                "也没有 TLS：请把它放在一个真正把这两件事做好的东西后面。有几条命令的行为"
                "和 Redis 不一样，有些还挺出人意料——<code>SCAN</code> 不是游标迭代器，"
                "<code>ZRANK</code> 是 O(N)，<code>SPOP</code> 不随机。"
                "<a href=\"docs/commands/\">每一条我们都写下来了</a>，写在 Redis 自己的"
                "命令参考里没有的那一栏。"
            ),
        },
        {
            "t": "steps",
            "h2": "三十秒",
            "intro": "三个入口。按你的数据待在哪里挑一个。",
            "items": [
                {
                    "title": "当服务端跑",
                    "body": "6379 端口上说 RESP。你的 redis-cli、你的客户端库、你现有的代码，一行都不用改。",
                    "code": "cargo install kevy\nkevy --port 6379",
                },
                {
                    "title": "放进你的 Rust 二进制",
                    "body": "没有 socket，没有进程。引擎就是一个 struct。",
                    "code": 'kevy-embedded = "4.0"\n\nlet db = Db::open("data/")?;\ndb.set(b"k", b"v", None)?;\nassert_eq!(db.get(b"k")?.as_deref(), Some(&b"v"[..]));',
                },
                {
                    "title": "放进浏览器标签页",
                    "body": "gzip 之后 151 KB。数据落在 OPFS 上，刷新页面还在，发布订阅可以跨标签页。",
                    "code": 'import { open } from "@goliajp/kevy";\n\nconst db = await open({ persist: { name: "app" } });\ndb.set("cart:u1", JSON.stringify(items), { ttlMs: 3600_000 });',
                },
            ],
        },
    ],
}

# ── /benchmarks/ ────────────────────────────────────────────────────────────

PAGES["benchmarks"] = {
    "title": "基准测试——kevy",
    "desc": "kevy 4.0 在同一台机器上对打 valkey 9.1、Redis 8 和 Dragonfly。测法、数字，以及我们花了一周才发现自己的压测工具一直在骗自己。",
    "foot": "这一页上的每个数字都能从 bench/ 里复现出来",
    "blocks": [
        {
            "t": "hero",
            "eyebrow": "基准测试",
            "h1": "数字，以及我们一开始是怎么把它测错的",
            "lede": (
                "这一页上的每个数字，都能用仓库里的 <code>bench/</code> 复现出来。"
                "不过更有用的是下面这段：那个让我们早期数字全部作废的错误、"
                "我们是怎么发现它的、以及我们改了什么，才让它不会再犯第二次。"
            ),
        },
        {
            "t": "callout",
            "kind": "warn",
            "title": "我们的压测工具一直在骗我们",
            "body": (
                "redis-benchmark 的吞吐量，是靠一个每 250 ms 触发一次的定时器报出来的"
                "（<code>SHOW_THROUGHPUT_INTERVAL</code>）。也就是说，它打印的每一个 rps 都是 "
                "<code>n / (0.25 s 的整数倍)</code>——而以 kevy 的速率，一整轮压测只够走几个 tick。"
                "我们旧表里 GET 和 SET 报出一位不差的相同数字，原因就在这儿；当时我们看见了，"
                "耸耸肩就过去了。那是量化。我们追着一个根本不存在的 5% 回归查了整整一周，"
                "才想起去读 <code>redis-benchmark.c</code>。下面的每一个数字，都改成从服务端自己的 "
                "<code>INFO stats</code> 里数出来，取三秒稳态窗口；压测脚本现在还会把参照 commit "
                "重新编译一份，和当前版本交替着跑，这样一台状态在漂的机器，也伪造不出一个结果来。"
            ),
        },
        {
            "t": "table",
            "h2": "四方对打，同一台机器",
            "intro": (
                "16 核，loopback，50 条连接，3 字节的 value。五次运行取中位数。这五次之间的离散度记在"
                "<a href=\"https://github.com/goliajp/kevy/blob/main/bench/PERF-LEDGER.md\">账本</a>里 —— "
                "每一行都是个位数百分比，不足以改变任何一条的排序。"
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
                "七战七胜。但 <b>LPUSH 只比 Redis 8 快 12%，ZADD 只快 10%</b>——在这个幅度上，"
                "你的 value 大小、你的 key 分布，比引擎本身更能决定结果。如果这两条正好是你的"
                "热命令，请拿你自己的负载去测，别拿我们的数字。这个颜色就是重点：凡是我们"
                "领先得不踏实的行，一律标成它，好让你扫一眼这张表，看到的是实情，而不是标题。"
            ),
        },
        {
            "t": "prose",
            "h2": "这组基准没告诉你的事",
            "body": [
                "它是一台机器、走 loopback、小 value。这等于把网络从等式里拿掉了，"
                "而在真实部署里，你真正在等的往往正是网络。如果你的延迟有 90% 花在网络上，"
                "那么 kevy 的 GET 比 valkey 快 2.6×，并不会让你的 p99 也好上 2.6×。",
                "value 一大，整张图就变形了。到 64 KB 一个 value 的时候，瓶颈整个挪到内核的 "
                "TCP 路径上，差距收窄到个位数百分比——perf 采样和复盘都放在 <code>bench/</code> 里，"
                "其中包括三个各自独立的优化：它们确实把用户态的 memcpy 实测降了下来，"
                "吞吐却一点没动，因为 memcpy 是一笔税，不是瓶颈。",
                "这个引擎是单机的。如果你的问题是一台机器不够用，kevy 解决不了，"
                "这一页上的任何数字都改变不了这一点。",
            ],
        },
        {
            "t": "table",
            "h2": "浏览器上的构建产物",
            "intro": "真正会被发进标签页的那些字节。",
            "head": ["", "体积", "说明"],
            "rows": [
                ["kevy.wasm", "416 KB", "引擎本体，未压缩"],
                ["gzip 之后", "151 KB", "真正过网络的量"],
                ["冷启动", "&lt; 20 ms", "编译加实例化，缓存已热"],
            ],
            "note": (
                "论小数据的读，localStorage 赢——它就是浏览器自己地址空间里的一个同步 map，"
                "任何建在 OPFS 之上的东西，在这一项上都赢不了它。kevy 赢的是别的地方，"
                "而那些地方，恰恰是 localStorage 本来就不该用的理由：有 TTL、没有 5 MB 的容量"
                "上限、存的是字节而不是字符串、写入不挡主线程。"
            ),
        },
        {
            "t": "code",
            "h2": "自己复现一遍",
            "caption": "这一页上的所有东西，都是这两个脚本跑出来的。",
            "text": "git clone https://github.com/goliajp/kevy && cd kevy\n\n# four-way arena: kevy, valkey, redis 8, dragonfly\nbash bench/arena.sh\n\n# the regression gate: rebuilds the reference commit and interleaves\nbash bench/perfgate.sh",
        },
    ],
}

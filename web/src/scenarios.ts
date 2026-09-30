import type { Lang } from './i18n'

// What the engine in the page can actually be asked to do.
//
// The landing page used to offer eight loose commands, and two of them
// were wrong — `INFO server` and an `IDX.CREATE` written from memory both
// answered "unknown command", on a page whose argument is that kevy does
// more than a KV store. Loose commands also under-sell it: `SET`/`GET`
// says nothing that a reader does not already assume.
//
// So the demo is scenarios. Each one is a short sequence that builds its
// own data and then asks a question of it, which is the only way a
// secondary index or a vector search means anything — one command in
// isolation cannot show a query answering over rows somebody just wrote.
//
// Every line here has been run against the engine and answers without an
// error; web/verify.mjs runs all of them in a browser on every push and
// fails the build on any error reply. The syntax comes from
// crates/kevy/src/cmd_index.rs and docs/cookbook.md rather than from
// memory — writing `IDX.CREATE … ON HASH PREFIX … FIELDS city` from
// memory is exactly how the broken chip got there.
//
// The engine here is the embedded engine with every feature a browser can
// host, streams and geo included. Transactions and scripting are outside
// the embedded surface on every platform, and a read that asks to BLOCK is
// refused (a tab has one thread to park), so no scenario uses them.

export type Scenario = {
  id: string
  /** The nav label — two or three words. */
  label: Record<Lang, string>
  /** One sentence: what the reader is about to watch happen. */
  blurb: Record<Lang, string>
  lines: string[]
}

export const SCENARIOS: Scenario[] = [
  {
    id: 'kv',
    label: { en: 'Keys & TTL', zh: '键与 TTL', ja: 'キーと TTL' },
    blurb: {
      en: 'The Redis surface, answered in this tab: strings, counters, expiry.',
      zh: '在这个标签页里作答的 Redis 面：字符串、计数器、过期。',
      ja: 'このタブが応答する Redis 面——文字列、カウンタ、期限。',
    },
    lines: [
      'SET greeting hello',
      'APPEND greeting " world"',
      'GET greeting',
      'STRLEN greeting',
      'INCRBY visits 7',
      'EXPIRE greeting 60',
      'TTL greeting',
      'TYPE greeting',
    ],
  },
  {
    id: 'types',
    label: { en: 'Structures', zh: '数据结构', ja: 'データ構造' },
    blurb: {
      en: 'Lists, hashes, sets and sorted sets — the same commands, the same replies.',
      zh: '列表、哈希、集合、有序集合 —— 同样的命令，同样的回复。',
      ja: 'リスト・ハッシュ・セット・ソート済みセット——同じコマンド、同じ応答。',
    },
    lines: [
      'LPUSH jobs "resize image" "send mail"',
      'LRANGE jobs 0 -1',
      'HSET user:2 name bob city osaka',
      'HGETALL user:2',
      'SADD tags rust wasm kv',
      'SMEMBERS tags',
      'ZADD board 42 alice 37 bob',
      'ZRANGEBYSCORE board 40 +inf WITHSCORES',
    ],
  },
  {
    id: 'index',
    label: { en: 'Secondary index', zh: '二级索引', ja: 'セカンダリ索引' },
    blurb: {
      en: 'Query hashes by a field instead of by key — range, filter, sort, and two indexes composed.',
      zh: '按字段查哈希，而不是按键 —— 范围、过滤、排序，以及两个索引的合成。',
      ja: 'キーではなくフィールドでハッシュを引く——範囲・絞り込み・並べ替え、そして二つの索引の合成。',
    },
    lines: [
      'HSET user:1 name alice city tokyo age 34',
      'HSET user:2 name bob city osaka age 41',
      'HSET user:3 name carol city tokyo age 29',
      'IDX.CREATE by_city ON PREFIX user: FIELD city TYPE str KIND range',
      'IDX.CREATE by_age ON PREFIX user: FIELD age TYPE i64 KIND range VALUES name city TYPES str str',
      'IDX.QUERY by_age RANGE 30 45 SORT city ASC FIELDS name city',
      'IDX.QUERY by_age RANGE 0 200 FILTER city EQ tokyo FIELDS name',
      'IDX.QUERY COMPOSE AND by_city EQ tokyo by_age RANGE 30 45 LIMIT 10 FIELDS name',
      'IDX.COUNT by_age RANGE 30 45',
    ],
  },
  {
    id: 'text',
    label: { en: 'Full text', zh: '全文检索', ja: '全文検索' },
    blurb: {
      en: 'An inverted index over a field, and a phrase query against it.',
      zh: '在字段上建倒排索引，再对它做短语查询。',
      ja: 'フィールド上の転置索引と、それに対するフレーズ検索。',
    },
    lines: [
      'HSET post:1 body "a pure rust key value store with no dependencies"',
      'HSET post:2 body "the engine compiled to wasm runs in the browser"',
      'IDX.CREATE search ON PREFIX post: FIELD body TYPE str KIND text',
      'IDX.QUERY search MATCH browser LIMIT 5 FIELDS body',
      'IDX.QUERY search MATCH "rust store" LIMIT 5 FIELDS body',
    ],
  },
  {
    id: 'vector',
    label: { en: 'Vector search', zh: '向量检索', ja: 'ベクトル検索' },
    blurb: {
      en: 'An HNSW index over an embedding field. Eight dimensions to stay readable; real ones are 768+.',
      zh: '在嵌入字段上建 HNSW 索引。这里用 8 维以便读懂，真实场景是 768 维起。',
      ja: '埋め込みフィールド上の HNSW 索引。読みやすさのため 8 次元、実際は 768 次元以上。',
    },
    lines: [
      'HSET mem:1 what "user prefers dark roast" v csv:0.9,0.1,0,0,0,0,0,0',
      'HSET mem:2 what "user asked about decaf" v csv:0.8,0.3,0.1,0,0,0,0,0',
      'HSET mem:3 what "coffee questions in the morning" v csv:0,0.2,0.9,0.1,0,0,0,0',
      'IDX.CREATE mem_ann ON PREFIX mem: FIELD v TYPE vector KIND ann DIM 8',
      'IDX.QUERY mem_ann KNN csv:0.85,0.2,0,0,0,0,0,0 LIMIT 2 FIELDS what',
    ],
  },
  {
    id: 'agg',
    label: { en: 'Aggregation', zh: '聚合', ja: '集計' },
    blurb: {
      en: 'Group-by maintained on write: counts, sums and extremes without scanning.',
      zh: '在写入时维护的 group-by：计数、求和、极值，不需要扫描。',
      ja: '書き込み時に維持される group-by——走査せずに件数・合計・極値。',
    },
    lines: [
      'HSET ord:1 cust alice amt 120',
      'HSET ord:2 cust bob amt 80',
      'HSET ord:3 cust alice amt 45',
      'IDX.CREATE ord_amt ON PREFIX ord: FIELD amt TYPE i64 KIND agg GROUPBY cust',
      'IDX.QUERY ord_amt GROUPS BY sum LIMIT 10',
      'IDX.QUERY ord_amt GROUP alice',
    ],
  },
  {
    id: 'view',
    label: { en: 'Views', zh: '视图', ja: 'ビュー' },
    blurb: {
      en: 'A named query kept up to date as the rows under it change.',
      zh: '一个具名查询，随它下面的行变化而保持最新。',
      ja: '下の行が変わるたびに最新に保たれる名前付きクエリ。',
    },
    lines: [
      'IDX.CREATE v_city ON PREFIX user: FIELD city TYPE str KIND range',
      'IDX.CREATE v_age ON PREFIX user: FIELD age TYPE i64 KIND range',
      'VIEW.CREATE tokyo_by_age QUERY v_city EQ tokyo ORDER BY v_age DESC',
      'VIEW.QUERY tokyo_by_age LIMIT 5',
      'VIEW.LIST',
    ],
  },
  {
    id: 'table',
    label: { en: 'Tables', zh: '表', ja: 'テーブル' },
    blurb: {
      en: 'Declare columns over a key prefix and the indexes come with it — rows stay ordinary hashes.',
      zh: '在键前缀上声明列，索引随之而来 —— 行仍然是普通的哈希。',
      ja: 'キー接頭辞に列を宣言すれば索引もついてくる——行は普通のハッシュのまま。',
    },
    // No query at the end: a freshly declared table's index answers
    // `-INDEXBUILDING` until it finishes, and the reply itself says to poll
    // IDX.LIST. Ending on a query would make this scenario's result depend
    // on how fast the machine is.
    lines: [
      'TABLE.DECLARE people PREFIX p: PK id COLUMN id i64 COLUMN name str COLUMN age i64 INDEX age range',
      'HSET p:1 id 1 name alice age 34',
      'HSET p:2 id 2 name bob age 41',
      'TABLE.LIST',
      'IDX.LIST',
    ],
  },
  {
    id: 'stream',
    label: { en: 'Streams', zh: 'Stream', ja: 'ストリーム' },
    blurb: {
      en: 'A stream with a consumer group: two workers share the entries, acknowledge what they finish, and one takes over what the other left.',
      zh: '带消费者组的 stream：两个 worker 分摊条目，做完的确认掉，一个接手另一个没做完的。',
      ja: 'コンシューマグループ付きのストリーム——二つのワーカーがエントリを分け合い、終えたものを確認し、片方が残したものをもう片方が引き取る。',
    },
    // Explicit ids so XACK can name one; `XADD orders * …` works the same.
    // No BLOCK: a tab has one thread, so a blocking read answers an error.
    lines: [
      'XGROUP CREATE orders shipping $ MKSTREAM',
      'XADD orders 1-0 sku A-1 qty 2',
      'XADD orders 2-0 sku B-7 qty 1',
      'XADD orders 3-0 sku C-3 qty 5',
      'XREADGROUP GROUP shipping worker-1 COUNT 2 STREAMS orders >',
      'XACK orders shipping 1-0',
      'XPENDING orders shipping',
      'XAUTOCLAIM orders shipping worker-2 0 0 COUNT 1',
      'XREADGROUP GROUP shipping worker-2 STREAMS orders >',
      'XPENDING orders shipping - + 10',
    ],
  },
  {
    id: 'geo',
    label: { en: 'Geo', zh: '地理位置', ja: '位置情報' },
    blurb: {
      en: 'Places by longitude and latitude: the distance between two, and everything within a radius.',
      zh: '按经纬度存地点：两点之间的距离，以及一个半径内的所有地点。',
      ja: '経度と緯度で場所を持つ——二点間の距離と、半径内のすべての場所。',
    },
    lines: [
      'GEOADD shops 139.7671 35.6812 tokyo-stn 139.7006 35.6896 shinjuku 135.4959 34.7025 osaka-umeda',
      'GEODIST shops tokyo-stn shinjuku km',
      'GEOSEARCH shops FROMLONLAT 139.75 35.68 BYRADIUS 10 km ASC WITHDIST',
      'GEOPOS shops osaka-umeda',
    ],
  },
  {
    id: 'scan',
    label: { en: 'Keyspace', zh: '键空间', ja: 'キー空間' },
    blurb: {
      en: 'Cursor iteration, bulk writes, and a digest that fingerprints every key under a prefix.',
      zh: '游标迭代、批量写入，以及给某个前缀下所有键取指纹的摘要。',
      ja: 'カーソル走査、一括書き込み、そして接頭辞配下の全キーを指紋化するダイジェスト。',
    },
    // Bitmaps were here first, and they run — in this tab. They are not in
    // the server's verb table (KNOWN_GAPS in kevy-resp/src/ops_table.rs
    // calls the family "unwired on RESP dispatch"), so a visitor who tried
    // BITCOUNT here and then against their own kevy would get "unknown
    // command". A demo should not teach a command the product does not
    // answer everywhere it claims to.
    lines: [
      'MSET a 1 b 2 c 3',
      'EXISTS a b nope',
      'SCAN 0 COUNT 5',
      'HSET user:1 city tokyo',
      'PREFIX.DIGEST user:',
      'DBSIZE',
    ],
  },
]

/** Every command in every scenario — what verify.mjs runs. */
export const ALL_LINES = SCENARIOS.flatMap((s) => s.lines)

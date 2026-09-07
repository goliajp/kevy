// Trilingual dictionary + a tiny hook. No i18n library: three locales, one
// landing page, a flat key space — a Record and a context are the whole
// machinery. The reference pages carry their own translations as markdown
// under docs/{zh,ja}/ and are checked by tools/check_doc_i18n.py.
//
// Each locale is written, not translated. The register is a technical
// project page: say what the thing does and what was measured, in the
// shortest form that stays precise. No defending against claims nobody
// made, no hedging, no filler connectives — those read as translationese
// in Chinese and Japanese and as padding in English.
//
// Every number here is measured and sourced. bench/REPORT.md holds the
// throughput figures (precision bench, n=1M x 10 runs); the command count
// comes from site/data/commands.json, which is generated from VERB_META.

import { createContext, useContext } from 'react'

export type Lang = 'en' | 'zh' | 'ja'

export const LANGS: { id: Lang; label: string }[] = [
  { id: 'en', label: 'EN' },
  { id: 'zh', label: '中文' },
  { id: 'ja', label: '日本語' },
]

export function detectLang(): Lang {
  const saved = localStorage.getItem('lang')
  if (saved === 'en' || saved === 'zh' || saved === 'ja') return saved
  const nav = navigator.language.toLowerCase()
  if (nav.startsWith('zh')) return 'zh'
  if (nav.startsWith('ja')) return 'ja'
  return 'en'
}

type Dict = Record<string, { en: string; zh: string; ja: string }>

const dict: Dict = {
  // ── masthead ──────────────────────────────────────────────────────
  'nav.try': { en: 'Try it', zh: '在线试用', ja: '試す' },
  'nav.speed': { en: 'Speed', zh: '性能', ja: '性能' },
  'nav.beyond': { en: 'Beyond Redis', zh: '超出 Redis 的部分', ja: 'Redis の先' },
  'nav.install': { en: 'Install', zh: '安装', ja: '導入' },
  'nav.docs': { en: 'Docs', zh: '文档', ja: 'ドキュメント' },

  // ── front matter ──────────────────────────────────────────────────
  'front.eyebrow': {
    en: 'Open source · Maintained by Golia Lab',
    zh: '开源 · 由 Golia Lab 承诺保持维护',
    ja: 'オープンソース · Golia Lab が継続的にメンテナンス',
  },
  'front.title.a': { en: 'A ', zh: '', ja: '' },
  'front.title.b': { en: 'Redis-compatible', zh: 'Redis 兼容', ja: 'Redis 互換' },
  'front.title.c': { en: ' engine that goes further', zh: '，但走得更远', ja: 'の、その先へ' },
  'front.abstract': {
    en: 'Your Redis client connects unchanged and every operation is faster — 2.5× on GET, 4.1× on SET against valkey 9.1, and ahead of Redis 8 on all seven verbs measured. What it adds is the rest of the data layer: vector search, full-text, secondary indexes, materialised views and a change feed, inside the engine rather than in four services around it. Pure Rust, no third-party crates, 46 of them. The terminal below is the real engine compiled to WebAssembly, running in this tab.',
    zh: '你的 Redis 客户端不用改一行就能连上，而每个操作都更快 —— 对 valkey 9.1，GET 快 2.5 倍、SET 快 4.1 倍；对 Redis 8，实测的七条命令全部领先。它多出来的是数据层的其余部分：向量检索、全文、二级索引、物化视图、变更流，全在引擎内部，而不是围着它的四个服务里。纯 Rust，零第三方 crate，共 46 个。下面这个终端是真引擎编译成 WebAssembly 后跑在你这个标签页里。',
    ja: 'お使いの Redis クライアントは一行も変えずに接続でき、しかも全操作が速い——valkey 9.1 に対して GET は 2.5 倍、SET は 4.1 倍。Redis 8 に対しても、計測した 7 コマンドすべてで上回ります。加えてデータ層の残りが揃います：ベクトル検索、全文検索、セカンダリインデックス、マテリアライズドビュー、チェンジフィード。周辺の四つのサービスではなく、エンジンの中に。純 Rust、サードパーティ crate ゼロ、全 46 crate。下のターミナルは本物のエンジンを WebAssembly にしたもので、このタブの中で動いています。',
  },
  'front.fig.speed': {
    en: 'SET, against valkey 9.1',
    zh: 'SET 吞吐，对 valkey 9.1',
    ja: 'SET スループット（valkey 9.1 比）',
  },
  'front.fig.commands': { en: 'commands', zh: '条命令', ja: 'コマンド' },
  'front.fig.deps': {
    en: 'third-party crates',
    zh: '个第三方 crate',
    ja: 'サードパーティ crate',
  },
  'front.fig.langs': { en: 'language bindings', zh: '种语言绑定', ja: '言語バインディング' },
  'front.cta.try': { en: 'Try it in your browser', zh: '在浏览器里试', ja: 'ブラウザで試す' },

  // ── terminal ──────────────────────────────────────────────────────
  'term.heading': { en: 'The engine, in this tab', zh: '引擎，就在这个标签页里', ja: 'このタブの中のエンジン' },
  'term.blurb': {
    en: 'Not a simulation and not a recording: this is kevy compiled to WebAssembly, holding real state in your browser. Type any command, or start from one below.',
    zh: '不是模拟，也不是录像：这是 kevy 编译成 WebAssembly 后在你浏览器里持有真实状态。可以随便敲命令，也可以从下面挑一条开始。',
    ja: 'シミュレーションでも録画でもありません。WebAssembly にコンパイルした kevy が、ブラウザ内で実際の状態を保持しています。任意のコマンドを入力するか、下のいずれかから始めてください。',
  },
  'term.caption.label': { en: 'Terminal.', zh: '终端。', ja: 'ターミナル。' },
  'term.caption': {
    en: 'The same binary the server runs, minus the network. State lives in this tab and disappears when you close it.',
    zh: '与服务端同一份二进制，只是去掉了网络。状态活在这个标签页里，关掉即消失。',
    ja: 'サーバーが動かすものと同じバイナリから、ネットワークだけを外したもの。状態はこのタブ内にあり、閉じると消えます。',
  },
  'term.scenarios': { en: 'What to run', zh: '跑哪一段', ja: '何を実行するか' },
  'term.pick': {
    en: 'Pick one — each builds its own rows, then asks a question of them.',
    zh: '挑一个 —— 每段先建自己的数据，再对它提问。',
    ja: 'ひとつ選ぶ——各段はまず自分のデータを作り、それに問いを立てます。',
  },
  'term.prompt': { en: 'type a command', zh: '输入命令', ja: 'コマンドを入力' },
  'term.reset': { en: 'clear', zh: '清空', ja: 'クリア' },
  'term.reach': {
    en: '112 of the 191 server verbs — the embedded surface. Arrow keys walk history; a pasted block runs line by line.',
    zh: '服务端 191 条动词中的 112 条 —— 嵌入式面。方向键翻历史，整段粘贴逐行执行。',
    ja: 'サーバーの 191 動詞のうち 112——組み込み面。矢印キーで履歴、貼り付けたブロックは一行ずつ実行。',
  },
  'term.booting': { en: 'starting engine…', zh: '正在启动引擎…', ja: 'エンジン起動中…' },
  'term.live': { en: 'live', zh: '运行中', ja: '実行中' },
  'term.failed': {
    en: 'the engine could not start in this browser',
    zh: '引擎无法在此浏览器中启动',
    ja: 'このブラウザではエンジンを起動できませんでした',
  },

  // ── speed ─────────────────────────────────────────────────────────
  'perf.heading': { en: 'Measured against valkey', zh: '与 valkey 实测对照', ja: 'valkey との実測比較' },
  'perf.blurb': {
    en: 'Same host, same client, same workload, alternating runs. Every cell is a precision bench — one million operations per run, ten runs, median reported.',
    zh: '同一主机、同一客户端、同一负载，交替执行。每一格都是精度基准 —— 每轮一百万次操作，十轮取中位数。',
    ja: '同一ホスト・同一クライアント・同一ワークロードを交互に実行。各セルは精密ベンチ——1 回あたり 100 万オペレーション、10 回の中央値。',
  },
  'perf.col.op': { en: 'Operation', zh: '操作', ja: '操作' },
  'perf.col.kevy': { en: 'kevy', zh: 'kevy', ja: 'kevy' },
  'perf.col.valkey': { en: 'valkey 9.1', zh: 'valkey 9.1', ja: 'valkey 9.1' },
  'perf.col.ratio': { en: 'Ratio', zh: '倍数', ja: '倍率' },
  'perf.caption.label': { en: 'Table 1.', zh: '表 1。', ja: '表 1。' },
  'perf.caption': {
    en: 'Throughput in operations per second, higher is better. Full method, hardware and the workloads where kevy does not win are in the benchmark report — a table that only showed the wins would not be a measurement.',
    zh: '吞吐量，单位为每秒操作数，越高越好。完整方法、硬件，以及 kevy 并未取胜的负载都在基准报告里 —— 只列胜场的表不叫测量。',
    ja: 'スループット(秒あたりオペレーション数、高いほど良い)。手法・ハードウェア・および kevy が勝っていないワークロードはベンチマークレポートに記載——勝ち星だけを並べた表は測定ではありません。',
  },
  'perf.report': { en: 'Full benchmark report', zh: '完整基准报告', ja: 'ベンチマーク全文' },

  // ── beyond redis ──────────────────────────────────────────────────
  'more.heading': { en: 'What Redis leaves to other services', zh: 'Redis 交给其它服务的那些事', ja: 'Redis が他のサービスに任せる領域' },
  'more.blurb': {
    en: 'Each of these usually means another process to run, another copy of the data, and a job to keep the two in step. Here they read the same keys the writes just landed in.',
    zh: '这些能力通常各自意味着再跑一个进程、再存一份数据，外加一个让两边保持同步的任务。在这里，它们读的就是写入刚落下的那批键。',
    ja: 'いずれも通常は、別プロセスをもう一つ動かし、データをもう一部持ち、両者を同期させるジョブを抱えることを意味します。ここではそれらが、書き込みが今落ちたのと同じキーを読みます。',
  },
  'more.vector.h': { en: 'Vector search', zh: '向量检索', ja: 'ベクトル検索' },
  'more.vector.p': {
    en: 'Approximate nearest neighbour over embeddings stored as ordinary values, filtered by ordinary keys.',
    zh: '在按普通值存储的向量上做近似最近邻，并可用普通键做过滤。',
    ja: '通常の値として保存した埋め込みに対する近似最近傍探索。通常のキーで絞り込めます。',
  },
  'more.fts.h': { en: 'Full text', zh: '全文检索', ja: '全文検索' },
  'more.fts.p': {
    en: 'Tokenised, scored search with CJK segmentation, over the values already in the store.',
    zh: '带评分的分词检索，支持中日韩切分，直接作用于已在库中的值。',
    ja: 'スコア付きのトークン検索。CJK の分かち書きに対応し、すでに格納済みの値を対象とします。',
  },
  'more.idx.h': { en: 'Secondary indexes', zh: '二级索引', ja: 'セカンダリインデックス' },
  'more.idx.p': {
    en: 'Declare a field, query by it. The index is maintained by the write path, so it cannot lag behind it.',
    zh: '声明一个字段，就能按它查询。索引由写路径自己维护，不可能落后于写入。',
    ja: 'フィールドを宣言すれば、それで問い合わせられます。インデックスは書き込みパス自身が維持するため、書き込みから遅れることがありません。',
  },
  'more.view.h': { en: 'Materialised views', zh: '物化视图', ja: 'マテリアライズドビュー' },
  'more.view.p': {
    en: 'A query whose result is kept current as the keys under it change, without a refresh job.',
    zh: '一条查询，其结果随下层键的变化保持最新，不需要刷新任务。',
    ja: '配下のキーが変わるたびに結果が最新に保たれるクエリ。リフレッシュジョブは不要です。',
  },
  'more.feed.h': { en: 'Change feed', zh: '变更流', ja: 'チェンジフィード' },
  'more.feed.p': {
    en: 'An ordered, replayable log of what changed, with a cursor — the thing people bolt a CDC pipeline on for.',
    zh: '一条有序、可重放、带游标的变更日志 —— 平常要为此外挂一整套 CDC 流水线。',
    ja: '順序付きで再生可能な変更ログとカーソル。通常はこのために CDC パイプラインを外付けします。',
  },
  'more.embed.h': { en: 'Embeddable', zh: '可嵌入', ja: '組み込み可能' },
  'more.embed.p': {
    en: 'The same engine as a library inside your process, over a C ABI. No server, no socket, same data files.',
    zh: '同一个引擎可作为库嵌入你的进程，走 C ABI。无服务端、无套接字，数据文件相同。',
    ja: '同じエンジンを C ABI 経由でプロセス内のライブラリとして。サーバーもソケットも不要で、データファイルは同一です。',
  },

  // ── install ───────────────────────────────────────────────────────
  'inst.heading': { en: 'Install', zh: '安装', ja: '導入' },
  'inst.server.blurb': {
    en: 'Run it as a server and point any Redis client at it. Nothing in your client code changes.',
    zh: '作为服务端跑起来，把任意 Redis 客户端指过来。客户端代码一行都不用改。',
    ja: 'サーバーとして起動し、任意の Redis クライアントを向けるだけ。クライアント側のコードは変わりません。',
  },
  'inst.embed.blurb': {
    en: 'Or embed it — no server, no socket, the same data files. Seven languages are on their registries; the Rust one is the engine itself.',
    zh: '也可以嵌入 —— 无服务端、无套接字，数据文件相同。七种语言已在各自的包管理器上；Rust 那一份就是引擎本体。',
    ja: '組み込みも可能です——サーバーもソケットも不要、データファイルは同一。七つの言語が各レジストリに公開済みで、Rust 版はエンジン本体そのものです。',
  },
  'inst.docs': {
    en: 'Every install line, every command, every configuration key:',
    zh: '每条安装命令、每条引擎命令、每个配置项：',
    ja: '各言語の導入手順・全コマンド・全設定キー：',
  },

  // The footer lives in components/Footer.tsx, shared with the reference
  // pages, and computes its year. A second copy of that string here is a
  // second thing to keep current.
}

export const LangContext = createContext<Lang>('en')

export function useLang() {
  return useContext(LangContext)
}

export function t(key: string, lang: Lang): string {
  const entry = dict[key]
  // A missing key is a bug, not a fallback: showing the key makes it
  // visible in every locale rather than silently serving English.
  if (!entry) return key
  return entry[lang]
}

const CJK = /[\u3040-\u30ff\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff\u3000-\u303f]/
const LATIN = /[0-9A-Za-z]/

/**
 * Chinese and Japanese line-breaking is the browser's job, and it does it
 * correctly: CJK folds at any character boundary — that is how CJK is
 * typeset — and `line-break: strict` in the stylesheet enforces kinsoku,
 * so a line never begins with 、 or ends with 「.
 *
 * The one thing the browser gets wrong is the space around embedded Latin
 * (盘古之白). That space is typographic, not lexical: 「IDX.QUERY 查询」and
 * 「二级索引」are single terms, and a fold there reads as a mistake. Make
 * exactly that space non-breaking and leave everything else alone — a space
 * between two Latin words ("Apple M4 Mac mini") is a real separator.
 *
 * Ported from tiktoken.golia.jp, where it already ran: the two lab pages
 * are one publication, and comparing the footers byte for byte is what
 * turned this up — the licence line read identically and differed, because
 * theirs had U+00A0 where ours had U+0020.
 *
 * Deliberately a plain string transform: emitting a wbr element or splitting into
 * nodes would override `line-break: strict` (a wbr element is an explicit break
 * opportunity, honoured even where kinsoku forbids one) and shred the text
 * into dozens of DOM nodes.
 */
export function phrase(text: string, lang: Lang): string {
  if (lang === 'en') return text
  return text.replace(/ /g, (_m, i: number) => {
    const a = text[i - 1]
    const b = text[i + 1]
    // A space at the edge of a fragment: the string is assembled around
    // an em or a code element, so the space exists only to set off the
    // Latin on the other side of the seam.
    if (!a || !b) return '\u00a0'
    const mixed = (CJK.test(a) && LATIN.test(b)) || (LATIN.test(a) && CJK.test(b))
    return mixed ? '\u00a0' : ' '
  })
}

export function T({ k }: { k: string }) {
  const lang = useLang()
  return <>{phrase(t(k, lang), lang)}</>
}

export { dict as DICT }

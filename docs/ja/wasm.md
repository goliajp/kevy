# WebAssembly上のkevy

kevyはブラウザの中で、コンパイルが通るだけの珍品ではなく本物のストアとして動きます。npmパッケージ[`@goliapkg/kevy`](https://www.npmjs.com/package/@goliapkg/kevy)は、`wasm32-unknown-unknown`向けにコンパイルしたエンジン（KV＋TTL＋カウンタ＋スキャン＋pub/sub）を手書きのESモジュールローダーの後ろに載せて出荷し、OPFS（IndexedDBフォールバック付き）による永続化と、タブをまたぐpub/subを備えます。同じクレート群は`wasm32-wasip1`向けにもビルドできるので、Rust APIは`wasmtime`／`wasmer`やエッジランタイムでも動きます。

ライブで試せます。[kevy.golia.jpのデモ](https://kevy.golia.jp/demo/)は、まさにこのモジュールの上のブラウザREPLです——コマンド、リロードを生き延びるOPFS永続化、タブ間pub/subを、バックエンドなしで。

## クイックスタート（ブラウザ）

```sh
npm install @goliapkg/kevy
```

```js
import { open } from "@goliapkg/kevy";

const db = await open({ persist: { name: "app" } });

db.set("greeting", "hello");
db.set("session", "abc123", { ttlMs: 60_000 });
db.getText("greeting");            // "hello"
db.incrby("visits");               // 1, 2, 3, ...
db.keys("user:*");

// Pub/sub — including other tabs of the same origin:
const off = db.subscribe("events", (payload, channel) => {
  console.log(channel, new TextDecoder().decode(payload));
});
db.publish("events", "hi from this or any other tab");

await db.flush();                  // durability barrier
```

書き込みはkevyの追記専用ログとしてストレージへストリームされ、同じ`persist.name`での次回`open()`時にリプレイされます。4.0以降、ログはチェックサム付きv2レコードフォーマット（`KEVYAOF2`——[persistence.md](persistence.md)参照）を話します：保存バイトのビット反転はリプレイ時に拒否され、黙って適用されることはありません。4.0以前のタブが保存したログはそのままリプレイされ（v1、永久に読める）、最初のcompactionでv2へ昇格します。ブラウザのタブから汲み出したログはネイティブのkevyで変わらずリプレイでき、逆も同様です。パッケージは6ファイル（パック後約165 KB）です。wasmモジュール、ローダー、OPFSワーカー、手書きのTypeScript型定義、そしていつものREADMEとマニフェスト。境界のどちら側も依存ゼロです。

## ローダーAPI

`open(options)`はモジュールをインスタンス化し、（永続化が有効なら）保存済みログをリプレイし、tickタイマーとタブ間ブリッジを開始します。オプションは次の通りです。

| オプション | デフォルト | 意味 |
|---|---|---|
| `wasm` | ローダーの隣の`kevy.wasm` | モジュールのソース。URL、`ArrayBuffer`、`Uint8Array`、`Response`、またはコンパイル済み`WebAssembly.Module` |
| `persist` | `false`（インメモリ） | `{ name, backend }`。`name`ごとに1つのログ。`backend`は`"auto"`（OPFS、IndexedDBフォールバック）、`"opfs"`、`"idb"` |
| `broadcast` | `true` | `BroadcastChannel`によるタブ間pub/subブリッジ |
| `name` | `persist.name`または`"kevy"` | インスタンス名。ストレージファイルとブロードキャストチャネルの両方をスコープする |
| `tickMs` | `100` | TTL掃除＋イベントポーリングの周期。`0`でタイマー無効——自分で`tick()`を呼ぶ |

返される`Kevy`ハンドル（完全なシグネチャは`kevy.d.ts`）：

| メソッド | 意味論 |
|---|---|
| `set(key, value, { ttlMs? })` | SET。オプションで期限付き |
| `get(key)` / `getText(key)` | GETを`Uint8Array`で／UTF-8テキストで。不在または期限切れなら`undefined` |
| `del(key)` / `exists(key)` | DEL / EXISTS |
| `expire(key, ttlMs)` / `persist(key)` / `pttl(key)` | PEXPIRE / PERSIST / PTTL（`-1`はTTLなし、`-2`はキーなし） |
| `incrby(key, delta?)` | INCRBY。新しい値を返す |
| `dbsize()` / `flushall()` | DBSIZE / FLUSHALL |
| `keys(pattern?, limit?)` | Redisグロブ付きKEYS。上限も指定可 |
| `tick()` | 手動のTTL掃除＋イベントポーリング1回。期限切れキー数を返す |
| `subscribe(channel, cb)` / `psubscribe(pattern, cb)` | SUBSCRIBE / PSUBSCRIBE。どちらも購読解除関数を返す |
| `publish(channel, payload)` | ローカル購読者へ、（ブリッジ有効なら）同一オリジンの他タブへもPUBLISH。ローカル受信者数を返す |
| `flush()` | 永続性バリア。保留中の書き込みフレームをストレージへフラッシュ |
| `compact()` | ストレージを、生きているキー空間のコンパクション済みイメージとして書き直す |
| `close()` | フラッシュし、ブリッジ／タイマー／ストレージを畳み、インスタンスを解放 |

キーと値はどこでも`string | Uint8Array | ArrayBuffer`です。文字列は境界でUTF-8エンコードされ、バイトビューはそのまま通ります——値は本物のバイナリであって、文字列ではありません。

## バインディングの仕組み（wasm-bindgen不使用）

kevyの依存ゼロの掟はツールチェーンにも及びます。境界のどちら側にもバインディングジェネレータはありません。[`kevy-wasm`](https://docs.rs/kevy-wasm)クレートはフラットな手書きC ABI——29個の`extern "C"`シンボル——をエクスポートし、`pkg/kevy.js`はTypedArray境界、UTF-8コーデック、永続化ポンプ、タブ間ブリッジを自前で持つ手書きローダーです。規約は次の通りです。

- **インスタンス**は`kevy_open`が返す`u32`ハンドルで、他のすべての呼び出しはハンドルを最初に取ります。`0`は決して有効なハンドルになりません。
- **入りのバイト列**は、`kevy_alloc`で確保し`kevy_free`で返す線形メモリを指す`(ptr, len)`対で渡ります。
- **出のバイト列**はインスタンスごとの結果バッファに置かれ、`kevy_out_ptr`／`kevy_out_len`で読みます。同じハンドルへの次の呼び出しまでが有効期間です——呼び出し側は即座にコピーアウトします。
- **ステータスコード**：`>= 0`が成功、`-1`が操作エラー（結果バッファにUTF-8メッセージ）、`-2`が不正ハンドル。
- **数値**は`f64`で渡ります（クロック、TTL、カウント——すべて2^53の正確な整数範囲の内側です）。
- `kevy_abi_version()`がABI契約のバージョンを報告するので、ローダーは不一致のモジュールを拒否できます。

エクスポート面はコア（open/close/alloc/free/clock/tick/結果バッファ）、KV（`kevy_set`、`kevy_get`、`kevy_del`、`kevy_exists`、`kevy_expire`、`kevy_persist`、`kevy_pttl`、`kevy_incrby`、`kevy_dbsize`、`kevy_flushall`、`kevy_keys`など）、pub/sub（`kevy_subscribe`、`kevy_psubscribe`、`kevy_unsubscribe`、`kevy_publish`、`kevy_poll_events`）、AOFポンプ（`kevy_aof_frames_out`、`kevy_aof_frame_in`、`kevy_aof_dump`）に分かれます。同梱のローダーがあなたのホストに合わないなら、このABIこそがサポートされる統合ポイントです——クレートのドキュメントがすべてのシンボルとパック済みバイトフォーマットを規定しています。

## 永続化——ネイティブkevyとバイト互換の本物のログ

ブラウザにファイルシステムはないので、永続性はホスト仲介です。永続化が有効なら、すべての書き込みは、kevyのAOFがディスクに保存するのと同じRESPマルチバルクフレーム（`kevy-persist`フォーマット——[persistence.md](persistence.md)）としてもエンコードされます。ローダーは保留中のフレームをマイクロタスクごとに1回ストレージへポンプするので、同期的な書き込みバーストのコストはストレージ追記1回です。`await db.flush()`が永続性バリアで、`flush()`の解決はバックエンドがディスクへフラッシュしたことを意味します。

**ブラウザのタブが書いたログは、そのままネイティブのkevyでリプレイできます**——同じマジックヘッダ、同じフレームです。`.aof`をOPFSからコピーしてネイティブの組み込みストア（またはサーバー）に向ければ、キー空間が戻ってきます。逆も成り立ちます。入りのポンプはネイティブが書いたログを受け付けます。壊れた末尾はネイティブのリプレイ契約に従います——無傷のプレフィックスが適用され、末尾は捨てられ、次のコンパクションがライブ状態からストレージを書き直します。

バックエンドは2つあり、`backend: "auto"`では自動選択されます。

- **OPFS**（第一候補）：小さな専用ワーカーが所有する`FileSystemSyncAccessHandle`——同期ハンドルはワーカー専用だからです。追記のたびに、確認応答の前にフラッシュします。
- **IndexedDB**（フォールバック）：同じポンプをオブジェクトストアに対して回します。OPFS同期ハンドルのないコンテキスト（特に古いSafari）向けです。

コンパクションは自動です。追記されたログが`max(512 KB, 前回イメージの4倍)`を超えると、ローダーはストレージを生きているキー空間のコンパクション済みイメージとして書き直します（AOFリライトのブラウザ側等価物）。`compact()`はスナップショットやエクスポートの前に1回を強制します。

**localStorageは意図的にバックエンドにしていません**。約5 MBのクォータ、書き込みのたびにメインスレッドをブロックする同期API、UTF-16文字列限定のストレージ——書き込みログとしては失格です。

## タブ間pub/sub

`publish()`は同じタブ内の購読者へはエンジン経由で配送し、（ブリッジ有効なら）インスタンスごとの`BroadcastChannel`でフレームを同一オリジンの他のすべてのタブへブロードキャストし、各タブのローカル購読者が受け取ります。配送は**at-most-once・バックログなし**です——メッセージを受け取るのはその時点で開いているタブだけで、後から開いたタブへのリプレイはありません。これはkevyサーバーのpub/sub（[pubsub.md](pubsub.md)）と同じ契約なので、片方の面に向けて書いたコードはもう片方でも同じように振る舞います。

## クロック、tick、TTL

`wasm32-unknown-unknown`にはスレッドもOSクロックもないので、エンジンは手動TTLリーパーとホスト供給クロックで動きます。ローダーは各エントリポイントの前に`Date.now()`をABI経由で供給し、タイマー（デフォルト100 ms）で`tick()`を回します。帰結は次の通りです。

- TTLの精度はtick周期に従います。500 msのTTLを持つキーは、期限後最初のtickで消えます。重要なら`tickMs`を詰めるか、`tickMs: 0`にして周期を完全に自分で握ってください。
- 呼び出しと呼び出しの間にバックグラウンド処理は一切起きません——隠れたコストはありませんが、（`tickMs: 0`で）`tick()`を忘れると期限切れキーが生き残り、イベントキューも掃けません。

## パフォーマンス

方法論・環境・生の数字の全体は[bench/WASM-BENCH.md](../../bench/WASM-BENCH.md)（ヘッドレスChromeハーネス、3回実行の中央値、16バイト値、1kおよび100kキー）にあります。Webアプリが実際に持っているストレージとの比較：

| 軸（n=100k） | kevy-wasm | vs IndexedDB | vs localStorage |
|---|---:|---:|---:|
| ポイント読み取り（ops/s） | 1.67 M | **77×** | 0.48× |
| ポイント書き込み（ops/s） | 1.79 M | **189×** | 4.9× |
| バッチロード（ms） | 60.8 | **36×速い** | 4.5×速い |
| スキャン、約10%一致（ms） | 5.6 | **46×速い** | 5.7×速い |
| 永続書き込み（ops/s） | 785 k（OPFS） | **12.6–17.4×** | 約2× |
| 再起動から利用可能まで（ms） | 129 | **2.7×速い** | 遅い（下記参照） |

ヘッドラインと、正直な但し書き：

- **IndexedDBに対して全軸で桁違い**——ポイント読み取り77–86×、ポイント書き込みはデータセットサイズをまたいで166–189×。
- **永続書き込みは素のIndexedDBの12.6–17.4×**。ポンプが書き込みフレームをマイクロタスク単位でバッチするので、書き込みバーストのコストはストレージ追記1回です。しかも本物の追記ログでありながら、localStorageのfire-and-forgetな`setItem`を約2×上回ります。
- **localStorageのポイント読み取りだけはkevyが勝てない列です**（0.42–0.48×）。そしてベンチは、wasmでホストされるストアには勝てない理由まで記録しています。ChromeのlocalStorage読み取りはレンダラーメモリ内のハッシュ検索で（実測3.5–5 M ops/sは素の`Map`級であって、ストレージ級ではありません）、一方どのwasmストアも呼び出しごとにUTF-8エンコード＋境界越え＋線形メモリからのコピーアウトを払います——その天井が約2–3 M ops/sで、kevyはローダーのステージング最適化（永続スクラッチバッファ、`encodeInto`、キャッシュ済みメモリビュー——素朴なローダー比2.2倍）の後、その近く（1.7–2.1 M）に座っています。kevyが勝つのは、localStorageがどんな速度でもできないことすべてです。バイナリ値、TTL、カウンタ、スキャン、5 MB超のデータセット、pub/sub、そしてノンブロッキングな永続化。
- **タブ間RTT**は64 KBペイロードでstorageイベントハックに勝ち（p50 0.295 ms対0.335 ms）、1 KB未満では約0.105 msのタスクホップ床で並びます——一方storageイベントハックは遅い消費者の下でフレームを落とし、文字列限定で、pingのたびにディスククォータへ書き込みます。

## Rustの顔——WASI、エッジランタイム、直接組み込み

エンジンそのもの——`kevy-embedded`とその依存クロージャ（`kevy-store`、`kevy-persist`、`kevy-hash`、`kevy-bytes`、`kevy-map`、`kevy-resp`）——は両方のwasmターゲットでビルドでき、CIがこのリスト全体＋`kevy-wasm`をプッシュごとにゲートします。ネットワークリアクタのクレート（`kevy-rt`、`kevy-sys`、`kevy-uring`）は意図的にクロージャの外に置かれ、wasmビルドはクリーンに保たれます。

| ターゲット | コマンド | 備考 |
|---|---|---|
| `wasm32-unknown-unknown` | `cargo build -p kevy-wasm --target wasm32-unknown-unknown --release` | ブラウザ成果物（`kevy_wasm.wasm`）。npmローダーがこれをインスタンス化する。独自ホストならC ABIを直接叩く。 |
| `wasm32-unknown-unknown`（Rust API） | `cargo build -p kevy-embedded --target wasm32-unknown-unknown` | スレッドなし・OSクロックなし。`Config::with_ttl_reaper_manual()`で開き、`set_clock_ns`／`set_wall_clock_ms`を供給し、ホストのループから`Store::tick()`を呼ぶ。 |
| `wasm32-wasip1` | `cargo build -p kevy-embedded --target wasm32-wasip1` | `Instant`／`SystemTime`が動く——クロック供給は不要。`std::fs`はpreopenしたディレクトリに対して動くので、`Config::with_persist("/data")`＋`wasmtime --dir=/data`で本物のAOF永続性が得られる。スレッドは依然ないので手動リーパーのまま。 |

`wasm32-unknown-unknown`でのRust直接組み込み：

```rust
use kevy_embedded::{Config, Store, set_clock_ns, set_wall_clock_ms};

let store = Store::open(Config::default().with_ttl_reaper_manual())?;

// Host feeds the clock before time-sensitive work…
set_clock_ns(now_ms_from_host().saturating_mul(1_000_000));
set_wall_clock_ms(now_ms_from_host());

store.set(b"hello", b"world")?;
let v = store.get(b"hello")?;   // Some(b"world".to_vec())

// …and drives expiry at its own cadence:
let _stats = store.tick();
```

エラーは`KevyResult`／`KevyError`で、argvスタイルの書き込みメソッドは借用スライスを取ります。ネイティブとまったく同じです——[`kevy-embedded`](https://docs.rs/kevy-embedded)のドキュメントを参照してください。

Cloudflare Workersなどのエッジアイソレートはブラウザのレシピに従います。アイソレートごとに1インスタンス、クロックソースは`Date.now()`、`tick()`は遅延実行かスケジュールされたハンドラから。アイソレート再起動をまたぐ永続性には、ハンドラからプラットフォームの永続ストアへ書き込みをミラーします（AOFポンプがフレームをくれます）。アイソレートの内側では、kevyがホットなインメモリ層です。

## FAQ

**ブラウザでフルのコマンド面は動きますか？** npmパッケージが公開するのはKV＋TTL＋カウンタ＋スキャン＋pub/subの切り出しです——ブラウザのストアに必要な面を、意図的に小さく保っています（wasmモジュールは非圧縮で約425 KB）。wasmターゲット上のRust APIは、コンパイル時に有効化した`kevy-embedded`フィーチャーの全面を公開します。

**永続化されたデータはポータブルですか？** はい——標準のkevy AOFです。ブラウザ→ネイティブも、ネイティブ→ブラウザも、どちらもリプレイできます。フォーマット契約は[persistence.md](persistence.md)を参照してください。

**共有メモリスレッド（`+atomics`）は？** 同梱モジュールはシングルスレッドで、これはブラウザ系のすべてのターゲットに合致します。ホストがスレッドを提供する場所ではエンジンはスレッドセーフですが、手動`tick()`モデルがサポートされるパスのままです。

**BroadcastChannelではなくSharedWorkerは？** 単一オーナーのSharedWorkerトポロジは理論上はきれいですが、利用可能な範囲が狭い（特にAndroid Chromeに不在）。BroadcastChannelブリッジが、同じ実測レイテンシ級のまま互換性で勝ちました。

**なぜwasm-bindgenを使わないのですか？** kevyはサーバーも組み込みもブラウザも、サードパーティ依存ゼロで出荷します。バインディングジェネレータはビルド時依存になり、境界レイアウトの所有権を持ってしまいます。手書きのABIなら、契約は明示的で、バージョン付きで、監査可能なままです。

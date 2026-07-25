# Luaスクリプティング

kevyにおけるサーバーサイドのLuaスクリプティングです。`EVAL` / `EVALSHA`でアトミックなスクリプトを走らせる方法、どんなバインディングがあるか、そして5.1より新しいLua方言をどうオプトインするかを扱います。

## このドキュメントが必要になるとき

次のようなことをしたいときに、Luaスクリプティングに手を伸ばしてください。

- 1つのキーに対して、小さなマルチコマンドの列をアトミックに実行する（check-then-set、条件つきカウンタ、分散ロック）。
- 単一のコマンドでは表現できないread-modify-writeのロジックをサーバーへ押し込み、往復を消す。
- エコシステムのライブラリ（BullMQ、Sidekiq、Bee Queue、Redlock、スライディングウィンドウのレートリミッタ）が公開しているスクリプトを、無改造で走らせる。

ふつうの単一コマンドのアクセスや、明示的な楽観ロックを伴う多数キーにまたがるトランザクションには、素のコマンドか`MULTI` / `EXEC`を使ってください。

## 中心となる考え方

`EVAL`はLuaのソース文字列をサーバーへ送り、コンパイルし、`KEYS[1]`を所有するシャード上で実行します。スクリプトが走っているあいだ、そのシャード上のほかのコマンドが割り込むことはありません——スクリプト全体が1つのアトミックな単位です。スクリプトの内側では、`redis.call("CMD", ...)`が通常のコマンドパスを通ってディスパッチし直され、`KEYS`と`ARGV`が入力への1始まり・バイナリセーフなアクセスを与え、スクリプトの戻り値はRESPへマーシャルされます。ロード済みのスクリプトはSHA1でキャッシュされるため、`SCRIPT LOAD` + `EVALSHA`により、クライアントは呼び出しのたびに本体ではなくハッシュを送れます。Luaランタイムは[luna](https://github.com/goliajp/luna)——純Rustの5.1〜5.5インタプリタです。デフォルトの方言はLua 5.1（Redisエコシステムのスクリプトが例外なく前提とするもの）であり、1行のshebangで、個々のスクリプトを5.2、5.3、5.4、5.5へオプトインさせられます。

## 動かしてみる例：上限つきカウンタ

カウンタを1つ増やしますが、増やした値が上限以下にとどまる場合に限ります。新しい値を返し、上限を超えてしまう場合は`nil`を返します。

```lua
-- KEYS[1] = counter key
-- ARGV[1] = cap (integer)
local cur = tonumber(redis.call("GET", KEYS[1]) or "0")
local cap = tonumber(ARGV[1])
if cur + 1 > cap then
  return nil
end
return redis.call("INCR", KEYS[1])
```

### `EVAL`によるインライン実行

```sh
redis-cli -p 6004 EVAL \
  "local cur = tonumber(redis.call('GET', KEYS[1]) or '0')
   local cap = tonumber(ARGV[1])
   if cur + 1 > cap then return nil end
   return redis.call('INCR', KEYS[1])" \
  1 quota:user:42 5
# (integer) 1
# … four more calls …
# (integer) 5
# next call:
# (nil)
```

### `SCRIPT LOAD` + `EVALSHA`によるキャッシュ実行

```sh
SHA=$(redis-cli -p 6004 SCRIPT LOAD \
  "local cur = tonumber(redis.call('GET', KEYS[1]) or '0')
   local cap = tonumber(ARGV[1])
   if cur + 1 > cap then return nil end
   return redis.call('INCR', KEYS[1])")
echo "$SHA"
# e.g. 7c3e0a9b1d4f...

redis-cli -p 6004 EVALSHA "$SHA" 1 quota:user:42 5
# (integer) 1

redis-cli -p 6004 SCRIPT EXISTS "$SHA"
# 1) (integer) 1
```

サーバーが一度も見たことのないハッシュで`EVALSHA`が送られてくると（コールドスタート、`SCRIPT FLUSH`のあとなど）、kevyは`-NOSCRIPT`を返すので、クライアントは`EVAL`にフォールバックすべきです。SHA1でキャッシュされるスクリプト本体にはshebang行も含まれるため、同じソースの5.1版と5.4版は、別々のキャッシュエントリになります。

### shebangによるLua 5.4方言

```lua
#!lua version=5.4
-- ARGV[1] = max retries before giving up
local tries = tonumber(ARGV[1])
local i = 0
::again::
i = i + 1
local ok = redis.call("SET", KEYS[1], "owned", "NX", "PX", 3000)
if type(ok) == "table" and ok.ok == "OK" then
  return i
end
if i < tries then goto again end
return redis.error_reply("LOCK_FAILED")
```

`goto`とラベル、それに整数型の算術は5.3以降の機能です。shebangは、このスクリプト1本だけを5.4のVMプールへ回し、ほかのスクリプトは5.1の上で走り続けます。

## バインディング

| シンボル | 振る舞い |
|---|---|
| `redis.call(cmd, ...)` | kevyのコマンドをディスパッチします。RESPのエラーはLuaのエラーを送出し、`pcall`で捕まえない限りスクリプトを中断します。 |
| `redis.pcall(cmd, ...)` | 同じディスパッチですが、RESPのエラーは送出せず`{err = "msg"}`として返ります。 |
| `redis.error_reply(msg)` | `{err = msg}`を作ります。スクリプトから返すと`-msg\r\n`にマーシャルされます。 |
| `redis.status_reply(msg)` | `{ok = msg}`を作ります。`+msg\r\n`（simple string）にマーシャルされます。 |
| `redis.sha1hex(s)` | 入力バイト列の、40文字・小文字16進のSHA-1。 |
| `redis.replicate_commands()` | 何もしません。すべてのスクリプトは、すでに1単位としてアトミックにレプリケートされます。 |
| `KEYS` | `EVAL`呼び出しで宣言された`numkeys`個のバイト文字列の、1始まりのテーブル。バイナリセーフ。 |
| `ARGV` | 残りの引数の、1始まりのテーブル。バイナリセーフ。 |
| `cjson.encode(v)` / `cjson.decode(s)` | 純RustのJSONコーデック。Redisの`cjson`ライブラリと同じ面。 |
| `cmsgpack.pack(v)` / `cmsgpack.unpack(s)` | 純RustのMessagePackコーデック。Redisの`cmsgpack`ライブラリと同じ面。 |

Luaの戻り値は、Redis標準の規則でRESPへマーシャルされます。`nil`と`false`はnil-bulkに、`true`は`:1`に、整数と誤差なく整数に落ちる浮動小数点数は整数応答に、それ以外の浮動小数点数と文字列はbulk stringに、`{ok=...}`はsimple stringに、`{err=...}`はエラー応答に、そして素の配列はmulti-bulk応答になります（最初のnilで打ち切る規則つき）。

## 方言の選択

| スクリプトの1行目 | 使われる方言 |
|---|---|
| （shebangなし） | Lua 5.1（デフォルト。BullMQ、Sidekiq、Redlock、そして公開されているRedis-Luaスニペットの大半が前提とするもの） |
| `#!lua version=5.1`（または`51`） | Lua 5.1、明示 |
| `#!lua version=5.2`（または`52`） | Lua 5.2 —— `goto`、`_ENV`、ephemeronテーブル |
| `#!lua version=5.3`（または`53`） | Lua 5.3 —— 整数サブタイプ、ビット演算子、`//`、`string.pack` / `string.unpack` |
| `#!lua version=5.4`（または`54`） | Lua 5.4 —— to-be-closed変数、整数`for`のセマンティクス、新しいビット演算の細部 |
| `#!lua version=5.5`（または`55`） | Lua 5.5 —— 公開されている最新の方言 |
| `#!lua version=<other>` | `-ERR unknown lua version: <X>` |

shebang行に載るRedis 7.0 Functionsのメタデータ（`flags=`、`name=`）はパースされたうえで無視されるため、Functionsの面向けに書かれたスクリプトも`EVAL`のもとで問題なくロードされます。

Redisエコシステムとの純粋な互換性だけを求めるデプロイでは、受け入れる方言をconfigで固定できます。`[lua] allow_dialects = "5.1"`は、shebangがそれより新しいVMを要求するスクリプトをすべて拒否します。複数を許すならカンマで区切ってください（`allow_dialects = "5.1,5.4"`）。デフォルト（空）はすべての方言を受け入れます。

## トレードオフと限界

- **ファイルシステム、ネットワーク、OSへのアクセスはありません。** `io`、`os`、`package`、`debug`、`coroutine`はロードされません。スクリプトはファイルを開くことも、ソケットを作ることも、プロセスを起こすことも、環境変数を読むこともできません。
- **バイトコードのロードはできません。** `load(bytecode)`と`string.dump`はブロックされます。VMに入れるのはLuaのソースだけであり、これが、歴史的にLuaのサンドボックスを破ってきたバイトコード検証器の抜け道を塞ぎます。
- **標準ライブラリはホワイトリスト制です。** `base`、`math`、`string`、`table`、`cjson`、`cmsgpack`が使えます。ほかの標準モジュールは存在しません。
- **スクリプトごとの時間予算。** 各`EVAL`は、`[lua] time_limit_ms`（デフォルト5000 ms ≈ 2億命令——Redisのデフォルトの`lua-time-limit`と同じ天井です。`0`で上限が外れます）から導かれた命令予算のもとで走ります。これを超えると、捕捉可能なLuaエラーを返してスクリプトを中断します。
- **スクリプトごとのメモリ予算。** 各スクリプトは、方言ごとのVMプールから種を取った、まっさらなインタプリタ状態の中で走ります。呼び出し中に作られたテーブルや文字列は、呼び出しが返るときに回収されます。呼び出しをまたいで共有される可変のLua状態は存在しません——何かを永続させたければkevyのキーを使ってください。
- **`EVAL`のネストはできません。** `redis.call("EVAL", ...)`を呼ぶスクリプトはエラーを返します。Redisの挙動と同じです。
- **スクリプト1本につき1シャード。** すべての`KEYS`が同じスロットにハッシュされなければなりません。シャードをまたぐ複数キーに触れるスクリプトは、ディスパッチの時点で`-CROSSSLOT`を受け取ります。
- **JITは設計上オフです。** インタプリタを直接使います。lunaのCranelift JITはkevyサーバーにリンクされていません。依存の面を最小に保ち、JIT時のポーズを避けるためです。

## FAQ

**手元にある既存のRedis Luaスクリプトは、無改造で動きますか？**
Lua 5.1（Redisのデフォルト）を対象にしていて、`redis.call` / `redis.pcall` / `redis.error_reply` / `redis.status_reply` / `KEYS` / `ARGV` / `cjson` / `cmsgpack`しか使っていないなら、動きます。debugライブラリ、OSライブラリ、あるいはプリコンパイルされたバイトコードのロードに依存しているなら、動きません——それらはサンドボックスの外に出されています。

**別々のシャードにあるキーをまたいでスクリプトを走らせるには？**
できません——アトミック性こそが要点だからです。仕事をシャードごとのスクリプトに分割してクライアント側で調整するか、関係するキーがハッシュタグを共有するようにキーのレイアウトを設計してください（`{user:42}:quota`と`{user:42}:counter`は同じシャードにルーティングされます）。

**`EVALSHA`が`-NOSCRIPT`を返します。どうすればいいですか？**
同じ本体を`EVAL`で送り直してください。サーバーはそれを再びキャッシュし、呼び出しに答えます。たいていのクライアントライブラリは、このフォールバックを自動で処理します。`SCRIPT FLUSH`もプロセスの再起動も、どちらもキャッシュをリセットします。

**スクリプトから`BLPOP`などのブロッキングコマンドを呼べますか？**
呼べません。`EVAL`の内側のブロッキングコマンドは、アトミック性の契約を破壊してしまいます——シャードは自分自身を待って凍りつくか、ほかの仕事を割り込ませるかのどちらかになるからです。ブロッキングコマンドは、スクリプトからディスパッチされるとエラーを返します。

**より新しいバージョンがあるのに、なぜデフォルトがLua 5.1なのですか？**
Redisエコシステムで公開されているスクリプトは、すべて5.1を前提にしています。5.1をデフォルトにするということは、それらのスクリプトが、整数サブタイプやビット演算子や`goto`まわりの驚きなしに、コピー&ペーストで通るということです。個々のスクリプトをshebangでより新しい方言へオプトインさせるのは、新しい機能が積極的に欲しくなったときの、1行の変更で済みます。

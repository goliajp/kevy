# `kevy-alloc`——サーバーのアロケータ

7.0 から、kevy サーバーはシステムアロケータ（Linux では glibc malloc）ではなく、kevy 独自の純 Rust の span アロケータ `kevy-alloc` で動きます。これは `kevy` crate の `kevy-alloc` feature で、デフォルトで有効です。そのため、サーバーのビルドはどれもこれを含みます：`cargo install kevy`、リリースのバイナリ、コンテナイメージ、`npm install -g @goliapkg/kevy-bin`。

グローバルアロケータとして組み込むのはサーバーのバイナリだけです。`kevy` crate をライブラリとして使うプログラムは、自分で宣言したアロケータをそのまま使います。この crate をリンクしても、割り当て方は変わりません。

## 何をするか

- **シャードごとに一つのヒープ、高速パスにロックなし。** シャードは自分のスレッドローカルなヒープから割り当てます。別のスレッドが解放したブロックは、持ち主のヒープに送り返されます。
- **ブロックにヘッダがない。** Rust は解放のたびにサイズを渡すので、アロケータはアドレスからブロックの span とサイズクラスを求められ、ブロックの隣には何も置きません。
- **ページを OS に返す。** 使用状況はデータページの外にあるビットマップに記録されるため、生きたブロックを含まない 4 KiB のページは、隣のページを使ったまま一枚ずつ返せます（`madvise(MADV_DONTNEED)`）。シャードの tick ごとに、しばらく空いたままのページを返します。glibc のメインアリーナは上端からしか縮められず、生きたチャンクの下にある空きチャンクは常駐したままです。
- **メモリの整理（コンパクション）。** 降格や削除された値は、元の場所に穴を残します。シャードの span 内の空き領域が生きたデータに比べて大きい間（生きたデータの 1/64 超かつ 4 MiB 超）、シャードの tick はアロケータが指す値をより密な span にコピーします。1 tick あたり 0.5 ms、空き領域が 1/16 を超えると最大 2 ms で、同じ tick の回収が空になったページを返します。空き領域が 1/256 を下回ると止まります。store が動かせない値（インデックスのリーフ、大きなコレクション）はそのままです。

## 測定結果

**スループット。** firefly（aarch64 Linux、4 KiB ページ）、同じ commit を両方の方法でビルド、サーバーを 2 コアに固定、`redis-benchmark -c 60`、二つのビルドを入れ替えながら 5 ラウンド。glibc に対する変化：

| コマンド | スループット |
|---|---:|
| `LPUSH` | +11.3 % |
| `ZADD` | +11.4 % |
| `HSET` | +3.1 % |
| `SADD` | +1.4 % |
| `GET`、`SET` | 同等 |
| `INCR` | −2.2 % |

`INCR` の値はノイズです。正確な命令数（callgrind、lx64、x86-64 Linux）では、1 コマンドあたり kevy-alloc で 2015 命令、glibc で 2016 命令です。同じ計測で、書き込みコマンドはどれも kevy-alloc での命令数が glibc 以下です。

**メモリ。** 階層化したサーバーは、常駐メモリを予算 × 1.05 以内に保ちます（[tiering.md](tiering.md) を参照）。D1 ワークロード（lx64、約 1 KiB のハッシュ 1,000 万件、3 GiB の予算、コンパイル済みインデックス 2 本）では、glibc は予算の約 3.5 % にあたる返せない穴を残します。整理のパスはそれを詰めて返します。

## 何が見えるか

`INFO modules` は、プロセスが実際に使っているアロケータを示します：

```
module:name=alloc,impl=kevy-alloc
```

`INFO allocator` は、アロケータがマップしたすべてのバイトを名前付きの項目に分け、各シャードのヒープで合計します：`alloc_live`（使用中）、`alloc_rounding`（サイズクラスへの切り上げ）、`alloc_cache`、`alloc_span_free`（使用中の span 内の空きスロット、整理が詰める対象）、`alloc_returned`（OS に返した分）、`alloc_virgin`（マップ済みで未使用）、`alloc_hysteresis`（空にして再利用のために保持）、`alloc_segment_overhead`。`alloc_accounted` はその合計で、`alloc_mapped` と一致します。

階層化のメモリガードはシステムヒープを走査せずにこれらの数値を読み、trim するヒープもありません：`INFO tiering` の `heap_trims_total` は 0 のままです。

## これを外してビルドする

```
cargo build --release -p kevy --bin kevy --no-default-features
cargo install kevy --no-default-features
```

`kevy-alloc` はこの crate の唯一のデフォルト feature なので、このビルドで変わるのはアロケータだけです。`module:name=alloc,impl=system` を報告し、`# Allocator` セクションはありません。階層化のガードは glibc の `mallinfo2`（macOS では malloc zone の統計）を読み、解放されたメモリがたまると `malloc_trim` でヒープを縮めます。

このビルドが必要になる場面：

- **malloc をフックするツール。** `LD_PRELOAD` で差し込むアロケータ（jemalloc、tcmalloc）、glibc の `MALLOC_*` チューニング、heaptrack や valgrind の memcheck が見るのは malloc の呼び出しです。デフォルトビルドのサーバーの割り当ては malloc を通らないため、これらのツールにはほとんど何も見えません。
- **4 KiB より大きいページ。** アロケータは 4 KiB 単位でページを返し、システムのページサイズがそれ以外なら一枚も返しません：Apple Silicon の macOS（16 KiB）や、16 KiB・64 KiB ページでビルドされた arm64 Linux カーネルです。そこでも解放したメモリは再利用しますが、OS には返しません。
- **比較。** 自分のワークロードをシステムアロケータと比べるなら、両方をビルドして同じマシンで動かします。

# IoTデバイス上のkevy

`kevy-embedded`は、サーバーがスケールアップするのと同じだけ意図的にスケールダウンします。フィーチャで階層化されたビルドは、およそ411 KBのインメモリKVコアから、インデックスとレプリケーションを含む全面までを覆います。Linux級のIoTボード（Pi Zero 2、OpenWrtルーター、産業用ARM）向けにはmuslのクロスビルドがあり、`no_std`コアはベアメタルのCortex-Mターゲット上で——コンパイルが通ることを確認しただけでなく——実際にブートし、動かされています。リソース予算はREADMEの約束ではなく、ゲートによって強制されます。

## フィーチャの階層

`kevy-embedded`はサブシステムごとにCargoフィーチャを1つ公開します。デフォルトは全部入りです。IoT向けのビルドは`core`から始めて、デバイスが実際にやることだけを足していきます。

| フィーチャ | 何が加わるか | 引き込むもの |
|---|---|---|
| `core` | KV + TTL + カウンタ + pub/sub + atomic/pipelineのファサード | （基底の面。実務上は常にオン。最小のアーキタイプを綴れるように名前が付いている） |
| `persist` | スナップショット + AOFの耐久性。`Config::with_persist`、`save_snapshot`、`rewrite_aof`、open時のAOFリプレイ | `kevy-persist` |
| `index` | 宣言型のセカンダリインデックス + materializedビュー | `kevy-index` |
| `text` | 全文（BM25）インデックスのセグメント | `index` + `kevy-text` |
| `vector` | ANN / HNSWのベクトルインデックスセグメント | `index` + `kevy-vector` |
| `replicate` | 組み込みをレプリカにする、組み込みをライターにする、そしてCDCフィード | `persist` + `kevy-replicate` + `kevy-resp` |
| `listener` | 読み取り専用のRESPリスナー（[docs/embedded-listener.md](embedded-listener.md)） | — |

階層間の依存関係はフィーチャ自身に符号化されています。`text`と`vector`は`index`を含意し、`replicate`は`persist`を含意します（レプリケートされたフレームはAOFのverbテーブルを通ってリプレイされるためです）。外したものは、リンカが落とす死んだコードになるのではありません——その背後にあるcrateは、そもそもビルドグラフに一切入りません。

6つのアーキタイプが、pushのたびにCIでコンパイルゲートされます。ワークスペースが動いても、それぞれの組み合わせがビルドされ続けるようにするためです。

```text
core                        # sensor cache: RAM-only KV + TTL
core,persist                # + survives power cycles
core,index                  # + declared indexes / views
core,index,text,vector      # + search (BM25, HNSW)
core,persist,replicate      # + edge node feeding a hub
core,listener               # + redis-cli-able diagnostics port
```

`Cargo.toml`ではこう書きます。

```toml
[dependencies]
kevy-embedded = { version = "4", default-features = false, features = ["core"] }
```

APIの顔はどの階層でも同じです。`Store::open(Config)`、`KevyResult` / `KevyError`のエラー、書き込みメソッドの借用スライスargv。`core`に対して書いたコードは、デバイスがのちに`persist`を得ても、そのまま再コンパイルできます。

## リソース予算（願望ではなく、ゲート済み）

予算のラインは2本あり、[`bench/iotgate.sh`](../../bench/iotgate.sh)がラチェットとして強制します。どちらの数値も、引き上げるには文書化された裁定が必要です。

| 予算 | ライン | 実測 |
|---|---|---|
| バイナリサイズ（`core`のコンシューマ、静的musl） | 600 KB以下 | **454 KB**（x86_64）・**411 KB**（aarch64）・**383 KB**（armv7） |
| `open`直後の空ストアのRSS（Linux） | 2 MB以下 | **336 KB**（aarch64） |

> これらは`bench/iot-consumer`上で計測しています——ワークスペースの**外側**に意図的に置いた、唯一の依存が`kevy-embedded`であるcrateです。それこそが、あなたが出荷する形です。このゲートの以前のバージョンはワークスペースの`--example`を測っていました。exampleのビルドはdev-dependencies（`kevy`サーバーcrate）を引き込むため、報告されるサイズがおよそ60%も水増しされていました。

`iot` cargoプロファイルはワークスペースのルートで定義されています——`release`のcodegenに、サイズを最優先させたものです。

```toml
[profile.iot]
inherits = "release"
opt-level = "z"     # size over speed
lto = true          # fat LTO: the linker drops unused subsystems
codegen-units = 1
strip = true
```

サイズの数値を再現するには、こうします。

```sh
cargo build --profile iot -p kevy-embedded --example iot_core \
  --no-default-features --features core
ls -l target/iot/examples/iot_core
```

## センサーキャッシュの全体像

`iot_core`のexampleは、たいていのデバイスが必要とする形です。TTL付きのインメモリKVと、手動で駆動する期限切れのsweepからなります（頼まない限り、バックグラウンドスレッドは存在しません）。

```rust
use kevy_embedded::{Config, Store};

fn main() -> kevy_embedded::KevyResult<()> {
    // Manual reaper: no thread is spawned; the device's own loop
    // drives expiry at whatever cadence it already runs.
    let s = Store::open(Config::default().with_ttl_reaper_manual())?;

    s.set(b"sensor:1", b"22.5")?;
    s.set(b"sensor:2", b"3.3")?;
    s.expire(b"sensor:2", core::time::Duration::from_secs(60))?;

    // Call from the main loop / timer ISR bottom half:
    let _expired = s.tick();
    Ok(())
}
```

スレッドが安く済むボードでは、リーパーはデフォルト（バックグラウンドスレッド）のままにしてください。ループを自分で持っているなら`with_ttl_reaper_manual()` + `tick()`を取ってください。TTLの精度はtickの刻みに追随します。

## クロスコンパイル：muslとCIマトリクス

静的muslのバイナリは、Linux級IoTにおけるデプロイの通貨です。ファイル1つ、glibcとの結合なし、`scp`して走らせるだけ。

### プラットフォームのカバレッジ——実際に走らせたもの

以下の行はどれも、コンパイルを確認しただけではなく、ビルドし、そして**実行**しました。`core`階層のコンシューマを、そのアーキテクチャの実カーネル上で走らせています。サイズは静的muslの成果物のものです。

| ターゲット | ボードの分類 | サイズ | 状態 |
|---|---|---|---|
| `x86_64-unknown-linux-musl` | 産業用x86、ゲートウェイ | 454 KB | **実行済み** |
| `aarch64-unknown-linux-musl` | Pi Zero 2 / Pi 4-5 / 多くのARM64 SBC | 411 KB | **実行済み——ネイティブに**。空ストアRSS 336 KB、2.86Mストアops/s |
| `armv7-unknown-linux-musleabihf` | Pi 2-3、旧来の32ビットARM | 383 KB | **実行済み**（エミュレート） |
| `arm-unknown-linux-musleabihf` | Pi Zero / Pi 1（ARMv6） | 411 KB | **実行済み**（エミュレート） |
| `riscv64gc-unknown-linux-musl` | RISC-V SBC | 420 KB | **実行済み**（エミュレート）——リンカとしてRISC-Vのクロスgccが必要（ターゲット仕様が`libgcc_s`を要求し、`rust-lld`はそれを供給できない）。加えて`crt-static`も要る |
| `thumbv7em-none-eabihf` | Cortex-M4/M7 MCU | ファームウェア145 KB | **ベアメタル上で実行済み**——`kevy-store`のみ。`kevy-embedded`の全面ではない（そちらは`std`を要します）。後述。 |

信頼できる性能値はaarch64の行です。この行はARM64上でネイティブに走りました。armv7/ARMv6の行は、コードが32ビットARM上で正しいことを証明していますが、その時間とRSSはエミュレータのオーバーヘッドを含んでおり、デバイスの数値として読むべきではありません。

出荷対象のターゲットはすべてCIでコンパイルゲートされ（pushのたびに、単独のコンシューマが各ターゲット向けにビルドされます）、Tier Aのターゲットについては**フル**のデフォルト面も検査されます。

```sh
rustup target add aarch64-unknown-linux-musl
cargo build --profile iot --target aarch64-unknown-linux-musl -p kevy-embedded

rustup target add armv7-unknown-linux-musleabihf
cargo build --profile iot --target armv7-unknown-linux-musleabihf -p kevy-embedded
```

kevyのゼロ依存の法が、ここで報われます。クロスコンパイルすべきCライブラリはなく、`-sys` crateの動物園もありません——OSの境界はkevy自身の`kevy-sys`だけであり、`core`より下の組み込みクロージャは、それすら含みません。

コンパイルの証明より先を見たいときは、エミュレーション下でテストスイートを走らせるのも1行です。

```sh
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER=qemu-aarch64 \
  cargo test -p kevy-embedded --target aarch64-unknown-linux-musl
```

## `no_std`コア

Linuxよりさらに下では、ストレージの石が`std`なしでビルドされます。5つのcrateが、`std`というデフォルトフィーチャの背後に`#![no_std]`のコアを抱えています——`kevy-store`、`kevy-hash`、`kevy-bytes`、`kevy-map`、`kevy-madvise`です。

CIは、それらをCortex-M向けにコンパイルするだけではありません。**その上でブートさせます**。`bench/mcu-probe`はベアメタルのファームウェアであり——手書きのベクタテーブル、bumpアロケータ、パニックハンドラ、semihostingのI/Oを備えています。kevyは依存を取らないので、`cortex-m-rt`や`embedded-alloc`に手を伸ばせないからです——これがQEMU（`mps2-an386`、Cortex-M4）上で本物の`Store`を立ち上げ、KVとTTLの面を動かします。pushのたびに走ります。MCU上でストアが誤動作すれば、ビルドが落ちます。

```sh
cd bench/mcu-probe && cargo run --release
```

そのMCU上で、64 KiBのヒープで計測した値です。

| | |
|---|---|
| ファームウェアイメージ | 145 KB |
| 空の`Store`のヒープ | **0 B** ——書き込むまで、ストアは何も確保しません |
| センサーキー64個分のヒープ | 17.8 KB |
| TTL | ファームウェアが自分の時計を進めれば、正しく期限切れになります |

**MCU上で走るのは`kevy-store`であって、`kevy-embedded`ではありません。** 組み込みのファサード（`Config`、バックグラウンドのリーパー、永続化、リスナー）は`std`を必要とします。MCUが手にするのは、その下にあるストレージエンジンを、直接駆動したものです。

`no_std`カットが実務上意味するのは、次のことです。

- **`alloc`は必須です。** ストアはヒープ上のデータ構造です。`#[global_allocator]`はあなたが用意します。`alloc`なしの階層は存在しません。
- **`external-clock`がOSの時計を置き換えます。** ホストが`set_clock_ns`（モノトニック）と`set_wall_clock_ms`（wall）を通じて時刻を供給します。ブラウザ向けビルドが使うのと同じ、ホスト供給の時計の契約です（[docs/wasm.md](wasm.md)）。
- **スレッドなし、ファイルなし、ソケットなし。** 永続化、レプリケーション、リスナーは、性質上`std`階層のフィーチャです。`no_std`のコアはインメモリのエンジンです。

1つだけ、知っておく価値のある細部があります。ほかの場所なら黙って壊れる類のものだからです。ホスト供給の時計は64ビットのセル1つであり、アトミックに読めなければなりません。64ビットアトミックを持つISAでは、これはただの`AtomicU64`です。32ビットしかないMCU（ARMv7E-Mに64ビットアトミックはありません）では、**2つの`AtomicU32`の半分にまたがる、単一ライターのseqlock**に退化します——読み手はちぎれた読み出しを検出するとリトライし、ホストが1つのコンテキストから時計を供給する限り、リトライループは即座に落ち着きます。複数のコンテキストから並行して時計を供給することは、契約の外です。

## サイジングの指針

- 上の数値は`core`階層のもの——典型値ではなく、床です。各能力が実際にいくらかかるかを、`iot`プロファイルのaarch64-muslで計測した値がこれです（その階層のAPIを本当に*呼ぶ*コンシューマでの計測です。使いもしないフィーチャフラグはLTOが消し去るので、コストはかかりません）。

  | 使うもの | バイナリ | 差分 |
  |---|---|---|
  | KV + TTL（`core`） | 392 KB | — |
  | + 耐久性（スナップショット + AOF） | 601 KB | **+209 KB** |
  | + RESPリスナー | 629 KB | +28 KB |
  | + セカンダリインデックス | 667 KB | +66 KB |
  | + 全文検索（BM25） | 691 KB | +24 KB |
  | + ベクトルANN（HNSW） | 696 KB | +29 KB |

  耐久性が、単独で群を抜いて最大の項目です。残りはその上に載る小さな増分にすぎません。空ストアのRSSも同じように動きます。`core`で336 KB、全フィーチャをコンパイルすると656 KBなので、選んだ階層は、ディスク上のバイト数以上に、常駐メモリに姿を現します。
- メモリはキー空間に比例して増えます。空ストアのRSS予算が存在するのは、まさに「kevyを*持っている*こと」の基礎コストを、あなたのデータの隣で取るに足らないものに保つためです。
- デバイスに診断ポートが必要なら、`listener`が`redis-cli`と話せる読み取り専用のRESPエンドポイントを与えてくれます（[docs/embedded-listener.md](embedded-listener.md)）。代償はスレッド1本とソケット1つ、そして書き込みは構造的に拒否されます。
- フラッシュ上での耐久性。`persist`はサーバーと同じAOF/スナップショット形式を書くので（[docs/persistence.md](persistence.md)）、デバイス上で書いたデータは、フリート内のどこででも読み戻せます。SDカード級のストレージ上でのAOF fsyncは、`Always`ではなく`EverySec`（デフォルト）を求めます。

## これは何ではないか

*サーバー*を小さくしようという試みは一切ありません。`kevy`（バイナリ）、`kevy-rt`、`kevy-uring`とその仲間たちは本物のカーネルを前提としており、IoTのカットではスコープ外です。ここでの製品は組み込みライブラリのほうです——あなたのファームウェアこそが、サーバーなのです。

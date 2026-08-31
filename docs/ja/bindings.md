# 各言語から kevy を使う

エンジンへの到達経路は二つあり、どちらを選ぶかでこのページの残りが決まります。

**ネットワーク越しに。** kevy のサーバーは RESP を話すので、その言語に既にある
Redis クライアントがそのまま接続できます——こちら側にインストールするものは
ありません。それが[お使いの Redis クライアントを向ける](../clients.md)であり、
たいていのサービスにとってはそれが正解です。

**プロセスの中に。** 同じエンジンが C ABI を持つライブラリになり、以下の
バインディングはそれを包んでいます。サーバーもソケットも不要で、データ
ファイルは同一。このページはその話です。

## 何がどこに公開されているか

以下の各行は、それぞれのレジストリから実際にインストールして動かしてから
書いています。

| 言語 | インストール | バージョン |
|---|---|---|
| Rust | `cargo add kevy-embedded` | 6.2.2 |
| Python | `pip install kevy` | 6.2.2 |
| Go | `go get github.com/goliajp/kevy-go/v6` | 6.2.2 |
| Java | Maven Central の `jp.golia:kevy` | 6.2.2 |
| Node / TypeScript | `npm i @goliapkg/kevy-ts` | 6.2.2 |
| ブラウザ（wasm） | `npm i @goliapkg/kevy` | 6.2.2 |
| Flutter | `flutter pub add flutter_kevy` | 6.2.2 |

```xml
<!-- Java、pom.xml に -->
<dependency>
  <groupId>jp.golia</groupId><artifactId>kevy</artifactId><version>6.2.2</version>
</dependency>
```

**.NET は現在 NuGet にありません。** 5.1.0 は公開後に非掲載としました。存在
しない内部パッケージへの依存を宣言しており、復元できなかったためです。修正は
リポジトリに入っており、次のリリースで出ます。それまでは `bindings/csharp`
からビルドしてください。

**Swift・Kotlin・C#・React Native** のバインディングは
[`bindings/`](https://github.com/goliajp/kevy/tree/develop/bindings) にあり、
ソースからビルドできますが、各レジストリにはまだ公開していません。

## 公開パッケージが持っているもの

多くの場合、**エンジンは入っていません**——これは意図的です。

エンジンはプラットフォームごとのネイティブライブラリです。Python の wheel、
Go の module、.NET のパッケージが通る配布経路はソースやマネージドコードを
運ぶもので、プラットフォームごと・バージョンごとに数十 MB を永久に抱えさせる
のは筋が通りません。そこで公開するのは「どこでも動く側」です。

- **Python と Go** は RESP クライアントを公開します。標準ライブラリ以外に
  依存がなく、その言語が動く場所すべてで動きます。`mem://` と `file://` は
  エンジンの入手先を示すエラーを返します。
- **Java・Node・ブラウザ・Flutter** はエンジンを同梱します。これらの
  パッケージ形式はもともとそのために作られているからです（JNI ライブラリを
  含む jar、`.node` アドオンを含む npm パッケージ、wasm モジュール、
  xcframework と jniLibs を含む pub パッケージ）。

エンジンが同梱されていない場合でも、コマンド一つの距離です。

```bash
git clone https://github.com/goliajp/kevy
cd kevy && cargo build --release -p kevy-ffi
```

あとはバインディングが探す場所に置くか、`KEVY_FFI_LIB` で指すだけです。

## 同じストアを、四つの言語から

```python
# Python —— pip install kevy
import kevy
db = kevy.connect("file:///var/lib/app")     # または kevy://host:6379
db.set(b"user:1", b"alice")
db.get(b"user:1")                            # b"alice"
```

```go
// Go —— go get github.com/goliajp/kevy-go/v6
import kevy "github.com/goliajp/kevy-go/v6"

c, _ := kevy.Connect("kevy://127.0.0.1:6379")
c.Set(ctx, []byte("user:1"), []byte("alice"))
```

```java
// Java —— jp.golia:kevy
try (KevyClient c = KevyClient.connect("file:///var/lib/app")) {
    c.set("user:1", "alice");
    c.get("user:1");
}
```

```dart
// Flutter —— flutter pub add flutter_kevy
final db = KevyDb.open('/data/app');
db.set('user:1', utf8.encode('alice') as Uint8List);
db.getText('user:1');                        // alice
```

いずれも同じエンジン、同じファイルです。Python バインディングが書いたストアは
Go バインディングで開けますし、サーバーでも開けます。

## どれを選ぶか

**すでに Redis と話しているサービス**にはバインディングは不要です。既存の
クライアントを kevy サーバーに向けるだけで、コードはそのままです。

**自分のデータを自分で持つアプリケーション**——デスクトップ、モバイル、CLI、
エージェント——には組み込み形態が向きます。動かすプロセスも占有するポートも
なく、データはそのままコピーできるディレクトリです。

**その中間**——背後に本物のデータ構造を持つローカルキャッシュが欲しい
サービスや、ネットワークが切れても動き続ける必要があるエッジプロセス——は、
エンジンを組み込んで中央から複製できます。[レプリケーション](replication.md)
を参照してください。

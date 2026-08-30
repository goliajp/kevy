# 在你的语言里使用 kevy

到达引擎有两条路，选哪条决定了这一页其余的一切。

**走网络。** kevy 的服务端说 RESP，所以你的语言里已有的 Redis 客户端不用改
就能连上 —— 我们这边没有东西需要你安装。那是[把你的 Redis 客户端指过来](../clients.md)，
对大多数服务来说就是正确答案。

**放进你的进程。** 同一个引擎编译成带 C ABI 的库，下面这些绑定包裹的就是它：
无服务端、无套接字，数据文件相同。这一页讲的是它。

## 已发布什么，在哪里

下表每一行都是先从各自的包管理器装下来跑通，再写下来的。

| 语言 | 安装 | 版本 |
|---|---|---|
| Rust | `cargo add kevy-embedded` | 6.1.0 |
| Python | `pip install kevy` | 6.1.0 |
| Go | `go get github.com/goliajp/kevy-go/v6` | 6.1.0 |
| Java | Maven Central 上的 `jp.golia:kevy` | 6.1.0 |
| Node / TypeScript | `npm i @goliapkg/kevy-ts` | 6.1.0 |
| 浏览器（wasm） | `npm i @goliapkg/kevy` | 6.1.0 |
| Flutter | `flutter pub add flutter_kevy` | 6.1.0 |

```xml
<!-- Java，写在 pom.xml 里 -->
<dependency>
  <groupId>jp.golia</groupId><artifactId>kevy</artifactId><version>6.1.0</version>
</dependency>
```

**.NET 目前不在 NuGet 上。** 5.1.0 那个包发出去了，现已下架：它声明依赖一个
不存在的内部包，因此还原不了。修复已在仓库里，随下一版发布。在那之前请从
`bindings/csharp` 自行构建。

**Swift、Kotlin、C# 和 React Native** 的绑定在
[`bindings/`](https://github.com/goliajp/kevy/tree/develop/bindings) 里，
可从源码构建，尚未上各自的包管理器。

## 发布出去的包里带什么

多数情况下**不带引擎** —— 这是有意的。

引擎是分平台的原生库。Python wheel、Go module、.NET 包所走的分发渠道发的是
源码或托管代码，要它们为每个平台、每个版本永久携带几十 MB 是不合适的。所以
发布的是"到处都能用"的那一半：

- **Python 和 Go** 发的是 RESP 客户端。它只依赖标准库，语言支持的平台它都
  支持。`mem://` 与 `file://` 会抛出一个指明去哪拿引擎的错误。
- **Java、Node、浏览器和 Flutter** 带引擎，因为它们的打包格式本来就是为此
  设计的（带 JNI 库的 jar、带 `.node` 插件的 npm 包、wasm 模块、带
  xcframework 与 jniLibs 的 pub 包）。

引擎不在包里的地方，它离你只有一条命令：

```bash
git clone https://github.com/goliajp/kevy
cd kevy && cargo build --release -p kevy-ffi
```

然后要么放在绑定会去找的位置，要么用 `KEVY_FFI_LIB` 指过去。

## 同一个库，四种语言

```python
# Python —— pip install kevy
import kevy
db = kevy.connect("file:///var/lib/app")     # 或 kevy://host:6379
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

它们是同一个引擎、同一批文件。Python 绑定写出的库，Go 绑定能打开，服务端也能。

## 该选哪一个

**已经在跟 Redis 说话的服务**一个绑定都不需要 —— 把现有客户端指向 kevy
服务端，代码原样保留。

**自己拥有数据的应用** —— 桌面端、移动端、CLI、agent —— 要的是嵌入形态。
没有进程要跑，没有端口要占，数据就是一个你可以直接拷走的目录。

**介于两者之间的** —— 想要一个背后有真数据结构的本地缓存的服务，或者必须在
网络断开时继续工作的边缘进程 —— 可以嵌入引擎并从中心节点复制。见
[复制](replication.md)。

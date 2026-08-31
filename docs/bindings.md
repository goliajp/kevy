# Use kevy from your language

There are two ways to reach the engine, and which one you want decides
everything else on this page.

**Over the network.** kevy's server speaks RESP, so the Redis client your
language already has connects to it unchanged — there is nothing of ours
to install. That is [Bring your redis client](clients.md), and for most
services it is the right answer.

**Inside your process.** The same engine compiles to a library with a C
ABI, and these bindings wrap it: no server, no socket, the same data
files. That is what this page is about.

## What is published, and where

Every line here was installed from its registry and run before it was
written down.

| Language | Install | Version |
|---|---|---|
| Rust | `cargo add kevy-embedded` | 6.2.1 |
| Python | `pip install kevy` | 6.2.1 |
| Go | `go get github.com/goliajp/kevy-go/v6` | 6.2.1 |
| Java | `jp.golia:kevy` on Maven Central | 6.2.1 |
| Node / TypeScript | `npm i @goliapkg/kevy-ts` | 6.2.1 |
| Browser (wasm) | `npm i @goliapkg/kevy` | 6.2.1 |
| Flutter | `flutter pub add flutter_kevy` | 6.2.1 |

```xml
<!-- Java, in pom.xml -->
<dependency>
  <groupId>jp.golia</groupId><artifactId>kevy</artifactId><version>6.2.1</version>
</dependency>
```

**.NET is not on NuGet.** The 5.1.0 package was published and is unlisted:
it declared a dependency on an internal package that does not exist, so it
could not be restored. The fix is in the repository and ships with the
next release. Until then, build it from `bindings/csharp`.

**Swift, Kotlin, C# and React Native** have bindings in
[`bindings/`](https://github.com/goliajp/kevy/tree/develop/bindings) that
build from source and are not on their registries yet.

## What a published package carries

Not the engine, in most cases — and that is deliberate.

The engine is a per-platform native library. Python wheels, Go modules and
.NET packages ship source or managed code through registries that would
have to carry tens of megabytes per platform, forever, for every version.
So the published package is the part that works everywhere:

- **Python and Go** publish the RESP client. It needs nothing but the
  standard library and works wherever the language does. `mem://` and
  `file://` raise an error naming where to get the engine.
- **Java, Node, the browser and Flutter** carry the engine, because their
  packaging formats are built to (a jar with a JNI library, an npm
  package with a `.node` addon, a wasm module, a pub package with an
  xcframework and jniLibs).

Where the engine is not in the package, it is one command away:

```bash
git clone https://github.com/goliajp/kevy
cd kevy && cargo build --release -p kevy-ffi
```

and either sits where the binding looks for it, or is pointed at with
`KEVY_FFI_LIB`.

## The same store, from four languages

```python
# Python — pip install kevy
import kevy
db = kevy.connect("file:///var/lib/app")     # or kevy://host:6379
db.set(b"user:1", b"alice")
db.get(b"user:1")                            # b"alice"
```

```go
// Go — go get github.com/goliajp/kevy-go/v6
import kevy "github.com/goliajp/kevy-go/v6"

c, _ := kevy.Connect("kevy://127.0.0.1:6379")
c.Set(ctx, []byte("user:1"), []byte("alice"))
```

```java
// Java — jp.golia:kevy
try (KevyClient c = KevyClient.connect("file:///var/lib/app")) {
    c.set("user:1", "alice");
    c.get("user:1");
}
```

```dart
// Flutter — flutter pub add flutter_kevy
final db = KevyDb.open('/data/app');
db.set('user:1', utf8.encode('alice') as Uint8List);
db.getText('user:1');                        // alice
```

Every one of them is the same engine and the same files. A store written
by the Python binding opens in the Go one, and both open in the server.

## Which to choose

**A service that already talks to Redis** wants no binding at all — point
your existing client at a kevy server and keep your code.

**An application that owns its data** — a desktop app, a mobile app, a
CLI, an agent — wants the embedded form. There is no process to run, no
port to hold, and the data is a directory you can copy.

**Something in between** — a service that wants a local cache with real
data structures behind it, or an edge process that must keep working when
the network does not — can embed the engine and replicate from a central
one. See [Replication](replication.md).

# kevy — native Java client

`jp.golia:kevy` — a first-party Java port of the language-agnostic
[kevy client contract](../../docs/client-contract.md). One artifact exposes
**both** a blocking and an asynchronous face, over **both** an embedded
in-process store and a remote RESP2/3 server. Zero runtime dependencies
beyond the JDK (JUnit 5 is test-only).

## One client, two faces, two backends

```java
import jp.golia.kevy.*;

// mem:// | file://  -> embedded (in-process, over the libkevy_jni door)
// kevy:// redis:// tcp:// -> remote RESP2/3 TCP
try (KevyClient c = Kevy.connect("kevy://127.0.0.1:6379")) {
    // blocking face
    c.set("k", "v");
    Optional<byte[]> v = c.get("k");
    long n = c.incr("counter");

    // async face — the SAME connection, results agree (§1.4)
    KevyAsyncClient a = c.async();
    CompletableFuture<Optional<byte[]>> f = a.get(Bytes.of("k"));
}
```

Every key/value/field is a binary-safe `byte[]` (the canonical form, §7);
`String` overloads (UTF-8) are provided across the common surface. The
mandatory raw escape hatch is `execute(...)` / `cmd(List<byte[]>)`, which
returns the `Reply` as data (an `-ERR` frame is **not** thrown there).

## Families

Core string/generic, hash, list, set, sorted-set, sorted-set algebra,
hash-field TTL, blocking pops (§3.1–3.7, §3.14) on **both** backends;
`Subscriber` pub/sub (§3.11), `Transaction` (§3.12, `AutoCloseable` →
implicit `DISCARD`), pipeline (§3.13), `IDX.*` (§3.8), `FEED.*` (§3.10) and
`ClusterClient` (§3.15) as detailed in the contract. Errors are a
`KevyException` hierarchy — one subclass per §2.2 variant, with `kind()` and
`storeError()` for value-style branching.

The embedded store is also usable directly (`EmbeddedDb`, §5.2):
`openMem()` / `open(dir)`, `cmd(argv)`, scalar `get`/`set`, subscribe poll.

## Build / test

Maven is the canonical build (`pom.xml`, group `jp.golia`, artifact `kevy`). The
coordinates are `jp.golia:kevy:5.1.0`, matching the engine version — but
**nothing is on Maven Central yet**, so resolve it locally
(`mvn install`) or use `run-tests.sh` below, which needs no Maven at all.
This repo needs neither Maven nor Gradle on `PATH`: `run-tests.sh` builds
`libkevy_jni` + the `kevy` server with cargo, compiles with `javac`, and
drives the JUnit 5 conformance suite through the
`junit-platform-console-standalone` jar (the same host-JVM path as
`bench/jnigate.sh`).

```sh
bash bindings/java/run-tests.sh          # both backends (remote spawns a real server)
```

The native `libkevy_jni` (from `crates/kevy-jni`) is loaded via
`System.loadLibrary("kevy_jni")`; point `-Djava.library.path` at the cargo
target dir holding `libkevy_jni.{dylib,so}`.

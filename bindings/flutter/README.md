# flutter_kevy

kevy embedded in **Flutter**, over `dart:ffi` — the real native engine
in your app, synchronous calls (the MMKV shape), plus everything MMKV
doesn't have: real TTL, hashes/lists/zsets, pub/sub, `cmd()` to every
verb, and persistence you can read (AOF + snapshots). A thin dart:ffi
layer over the same kevy-ffi C ABI every other door wraps — the engine
ships as a jniLib on Android and a linked XCFramework on iOS.

```dart
import 'package:flutter_kevy/flutter_kevy.dart';
import 'package:path_provider/path_provider.dart';

final docs = await getApplicationDocumentsDirectory();
final db = KevyDb.open('${docs.path}/kevy'); // or KevyDb.openInMemory()

db.setText('session:7f3a', 'payload', ttlMs: 3600000); // scalar fast path
db.getText('session:7f3a'); // 'payload'

final sub = db.subscribe('room');
db.publish('room', utf8.encode('hi'));
final frame = sub.next(); // poll; a RESP-array value or null

// The escape hatch: every verb, RESP semantics, errors as VALUES.
final reply = db.cmd(['ZADD', 'board', '42', 'alice']);

db.close();
```

`open()` and `set`/`get` are synchronous — that is the MMKV lane. Typed
methods (`setText` / `getText` / `del` / `incrBy` / `expire` / `pttlMs`
/ `keys` / `dbSize` / `flushAll` / `publish` / `subscribe`) throw
`KevyError` on a protocol error; `cmd()` returns it as a value. Same API
shape as every other kevy embedding (Node/Bun, .NET, Swift, Kotlin,
expo, wasm).

The bindings are generated from `crates/kevy-ffi/include/kevy.h` by
ffigen; `scripts/prepare-native.sh` vendors the engine (build it first
with `packaging/apple/build-xcframework.sh` and
`packaging/android/build-ffi-jnilibs.sh`). Docs: <https://kevy.golia.jp>.

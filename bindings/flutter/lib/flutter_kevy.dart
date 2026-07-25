// flutter_kevy — kevy embedded in Flutter, over dart:ffi.
//
// The same one stone every other door wraps: the generated bindings
// (flutter_kevy_bindings_generated.dart, from crates/kevy-ffi/include/
// kevy.h) call the C ABI directly, and this file is the typed surface —
// mirroring the wasm / node / swift / kotlin packages, with cmd() as the
// escape hatch to every verb. Typed methods THROW on a protocol error;
// cmd() returns KevyError as a value.
//
//   final db = KevyDb.open('${dir.path}/kevy');
//   db.setText('session:7f3a', 'payload', ttlMs: 3600000);
//   db.getText('session:7f3a'); // 'payload'
//   db.close();
import 'dart:convert';
import 'dart:ffi' as ffi;
import 'dart:io' show Platform;
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

import 'flutter_kevy_bindings_generated.dart';
import 'src/resp.dart';

export 'src/resp.dart' show KevyError, respText;

// The engine is the kevy-ffi cdylib. Android ships it as a jniLib
// (libkevy_ffi.so, dlopen by name); iOS embeds it as a DYNAMIC framework
// and loads it by name (hence DynamicLibrary.open below, not .process()) —
// macOS links the static KevyKit stone into the host process image.
final ffi.DynamicLibrary _lib = () {
  if (Platform.isAndroid) return ffi.DynamicLibrary.open('libkevy_ffi.so');
  if (Platform.isIOS) return ffi.DynamicLibrary.open('kevy_ffi.framework/kevy_ffi');
  if (Platform.isMacOS) return ffi.DynamicLibrary.process();
  if (Platform.isLinux) return ffi.DynamicLibrary.open('libkevy_ffi.so');
  if (Platform.isWindows) return ffi.DynamicLibrary.open('kevy_ffi.dll');
  throw UnsupportedError('kevy: unsupported platform');
}();

final FlutterKevyBindings _b = FlutterKevyBindings(_lib);

// Leak backstops: if close() is forgotten, the GC still drops the native
// store / subscription (and its store, AOF, bus registration). Both take a
// single opaque handle pointer, so they bind straight to a NativeFinalizer.
// (kevy_buf_free_shared can't — it takes three scalars, not one pointer, so
// getView() would need a shim; see get() for why we keep the copy instead.)
final ffi.NativeFinalizer _dbFinalizer =
    ffi.NativeFinalizer(_lib.lookup<ffi.NativeFinalizerFunction>('kevy_close'));
final ffi.NativeFinalizer _subFinalizer = ffi.NativeFinalizer(
    _lib.lookup<ffi.NativeFinalizerFunction>('kevy_sub_close'));

/// A pub/sub subscription. [next] drains one frame without blocking (or
/// null when the queue is empty); [waitNext] parks in the kernel until a
/// frame arrives or the timeout elapses; [close] unsubscribes.
class KevySub implements ffi.Finalizable {
  ffi.Pointer<KevySubHandle> _p;
  KevySub._(this._p) {
    if (_p != ffi.nullptr) _subFinalizer.attach(this, _p.cast(), detach: this);
  }

  Object? next() {
    if (_p == ffi.nullptr) return null;
    final out = calloc<KevyBuf>();
    try {
      final rc = _b.kevy_sub_next(_p, out);
      if (rc < 0) throw KevyError('kevy: subscription misuse');
      if (rc == 0) return null;
      return _takeBuf(out.ref);
    } finally {
      calloc.free(out);
    }
  }

  /// Block up to [timeoutMs] (0 = forever) for one frame, parking in the
  /// kernel instead of spinning [next]. Returns null on timeout / bus-gone.
  Object? waitNext(int timeoutMs) {
    if (_p == ffi.nullptr) return null;
    final out = calloc<KevyBuf>();
    try {
      final rc = _b.kevy_sub_wait(_p, timeoutMs, out);
      if (rc < 0) throw KevyError('kevy: subscription misuse');
      if (rc == 0) return null;
      return _takeBuf(out.ref);
    } finally {
      calloc.free(out);
    }
  }

  void close() {
    if (_p != ffi.nullptr) {
      _subFinalizer.detach(this);
      _b.kevy_sub_close(_p);
    }
    _p = ffi.nullptr;
  }
}

/// The embedded engine. One instance owns its data directory.
class KevyDb implements ffi.Finalizable {
  ffi.Pointer<KevyDbHandle> _p;
  KevyDb._(this._p) {
    if (_p != ffi.nullptr) _dbFinalizer.attach(this, _p.cast(), detach: this);
  }

  /// Open a persistent store rooted at [dir] (a plain path). Throws on
  /// failure.
  static KevyDb open(String dir) {
    final bytes = utf8.encode(dir);
    final p = malloc<ffi.Uint8>(bytes.length);
    try {
      p.asTypedList(bytes.length).setAll(0, bytes);
      final db = _b.kevy_open(p, bytes.length);
      if (db == ffi.nullptr) throw KevyError('kevy: open failed');
      return KevyDb._(db);
    } finally {
      malloc.free(p);
    }
  }

  /// [open] with explicit durability/rewrite policy. `fsync`: 0 everysec
  /// (default) / 1 always / 2 no. `rewritePct`/`rewriteMinSize` are the
  /// classic growth pair; `rewriteBytes` (absolute cap) and
  /// `rewriteIntervalSecs` (staleness) each 0 = off — they exist because
  /// the growth rule alone lets a large log double before compacting.
  static KevyDb openWith(
    String dir, {
    int fsync = 0,
    int shards = 0,
    int rewritePct = 100,
    int rewriteMinSize = 64 * 1024 * 1024,
    int rewriteBytes = 0,
    int rewriteIntervalSecs = 0,
  }) {
    final bytes = utf8.encode(dir);
    final p = malloc<ffi.Uint8>(bytes.length);
    final opts = malloc<KevyOpenOptions>();
    try {
      p.asTypedList(bytes.length).setAll(0, bytes);
      opts.ref.fsync = fsync;
      opts.ref.shards = shards;
      opts.ref.rewrite_pct = rewritePct;
      opts.ref.rewrite_min_size = rewriteMinSize;
      opts.ref.rewrite_bytes = rewriteBytes;
      opts.ref.rewrite_interval_secs = rewriteIntervalSecs;
      final db = _b.kevy_open_with(p, bytes.length, opts);
      if (db == ffi.nullptr) throw KevyError('kevy: open failed');
      return KevyDb._(db);
    } finally {
      malloc.free(p);
      malloc.free(opts);
    }
  }

  /// Flush every shard's AOF with a REAL fsync, then refuse every later
  /// write (reads stay available). Idempotent — the deterministic teardown
  /// for a lifecycle hook: `db.shutdown(); exit(0)`.
  void shutdown() {
    final rc = _b.kevy_shutdown(_live);
    if (rc != 0) throw KevyError('kevy: shutdown failed (rc=$rc)');
  }

  /// Open a pure in-memory store — nothing survives the process.
  static KevyDb openInMemory() {
    final db = _b.kevy_open_mem();
    if (db == ffi.nullptr) throw KevyError('kevy: open failed');
    return KevyDb._(db);
  }

  /// The engine version.
  static String version() => _b.kevy_version().cast<Utf8>().toDartString();

  ffi.Pointer<KevyDbHandle> get _live {
    if (_p == ffi.nullptr) throw KevyError('kevy: closed handle');
    return _p;
  }

  // ── the escape hatch: every verb, RESP semantics, errors as values ──
  Object? cmd(List<Object> argv) {
    final db = _live;
    final args = argv
        .map<Uint8List>(
            (a) => a is Uint8List ? a : utf8.encode(a.toString()))
        .toList();
    final ptrs = malloc<ffi.Pointer<ffi.Uint8>>(args.length);
    final lens = malloc<ffi.Size>(args.length);
    final bufs = <ffi.Pointer<ffi.Uint8>>[];
    final out = calloc<KevyBuf>();
    try {
      for (var i = 0; i < args.length; i++) {
        final n = args[i].length;
        final p = malloc<ffi.Uint8>(n == 0 ? 1 : n);
        if (n > 0) p.asTypedList(n).setAll(0, args[i]);
        bufs.add(p);
        ptrs[i] = p;
        lens[i] = n;
      }
      final rc = _b.kevy_cmd(db, args.length, ptrs, lens, out);
      if (rc != 0) throw KevyError('kevy: kevy_cmd misuse');
      return _takeBuf(out.ref);
    } finally {
      for (final p in bufs) {
        malloc.free(p);
      }
      malloc.free(ptrs);
      malloc.free(lens);
      calloc.free(out);
    }
  }

  // ── the scalar fast path: no argv assembly, no RESP framing ─────────
  /// Store [value] at [key], optionally expiring after [ttlMs] ms
  /// (0 = no TTL). Overwrites any existing value.
  void set(String key, Uint8List value, {int ttlMs = 0}) {
    final k = utf8.encode(key);
    final kp = malloc<ffi.Uint8>(k.length);
    final vp = malloc<ffi.Uint8>(value.isEmpty ? 1 : value.length);
    try {
      kp.asTypedList(k.length).setAll(0, k);
      if (value.isNotEmpty) vp.asTypedList(value.length).setAll(0, value);
      final rc = _b.kevy_set(
          _live, kp, k.length, vp, value.length, ttlMs > 0 ? ttlMs : 0);
      if (rc != 0) throw KevyError('kevy: kevy_set misuse');
    } finally {
      malloc.free(kp);
      malloc.free(vp);
    }
  }

  /// [set] with a UTF-8 string value.
  void setText(String key, String value, {int ttlMs = 0}) =>
      set(key, utf8.encode(value), ttlMs: ttlMs);

  /// The value at [key] as raw bytes, or null on a miss. Throws a typed
  /// [KevyError] (WRONGTYPE) if [key] holds a non-string type.
  Uint8List? get(String key) {
    final k = utf8.encode(key);
    final kp = malloc<ffi.Uint8>(k.length);
    final out = calloc<KevyBuf>();
    try {
      kp.asTypedList(k.length).setAll(0, k);
      // Zero-copy shared lane: a bulk value comes back as an Arc clone (a
      // refcount bump, no engine-side Vec copy) that this buffer VIEWS. NOT a
      // measured win here — Dart's get already used the scalar kevy_get, not
      // RESP; the delta is only the eliminated engine Vec copy on large
      // values, unbenchmarked. Adopted for parity with the sibling doors.
      final rc = _b.kevy_get_shared(_live, kp, k.length, out);
      // The shared lane rejects a non-string key with a negative code. Re-run
      // through the framed GET so a -WRONGTYPE reply surfaces as a typed
      // KevyError instead of an opaque "misuse" (mirrors the C++/Client doors).
      if (rc < 0) return _getFramedFallback(key);
      if (rc == 0) return null;
      // The scalar fast path returns RAW value bytes, not a RESP reply —
      // take them directly, do NOT parse.
      final b = out.ref;
      // Empty-string hit: ptr may be null, so never deref it via asTypedList.
      if (b.len == 0) {
        _b.kevy_buf_free_shared(b.ptr, b.len, b.cap);
        return Uint8List(0);
      }
      final bytes = Uint8List.fromList(b.ptr.asTypedList(b.len));
      // Pairs 1:1 with kevy_get_shared — the shared free drops the Arc/Vec
      // owner (cap is its opaque handle); never kevy_buf_free on this buffer.
      _b.kevy_buf_free_shared(b.ptr, b.len, b.cap);
      return bytes;
    } finally {
      malloc.free(kp);
      calloc.free(out);
    }
  }

  // Re-issue GET through the framed path when the scalar lane rejected the
  // key, so an engine-level error (a -WRONGTYPE on a non-string key) surfaces
  // as a typed KevyError; a genuine value/miss still resolves normally.
  Uint8List? _getFramedFallback(String key) {
    final v = cmd(['GET', key]);
    if (v is KevyError) throw v;
    if (v == null) return null;
    if (v is Uint8List) return v;
    if (v is String) return Uint8List.fromList(utf8.encode(v));
    throw KevyError('kevy: kevy_get misuse');
  }

  /// [get] decoded as UTF-8 text, or null on a miss.
  String? getText(String key) {
    final v = get(key);
    return v == null ? null : utf8.decode(v);
  }

  // ── the typed surface (mirrors the other kevy packages) ─────────────
  /// Delete [keys]; returns how many existed.
  int del(List<String> keys) => _want(cmd(['DEL', ...keys])) as int;

  /// Atomically add [delta] to the integer at [key]; returns the new value.
  int incrBy(String key, int delta) =>
      _want(cmd(['INCRBY', key, '$delta'])) as int;

  /// Set [key]'s TTL to [ttlMs] ms; returns true if the key existed.
  bool expire(String key, int ttlMs) =>
      _want(cmd(['PEXPIRE', key, '$ttlMs'])) == 1;

  /// Remaining TTL of [key] in ms (-1 = no TTL, -2 = no such key).
  int pttlMs(String key) => _want(cmd(['PTTL', key])) as int;

  /// Keys matching the glob [pattern] (default all).
  List<String> keys([String pattern = '*']) {
    final v = _want(cmd(['KEYS', pattern]));
    if (v is! List) return [];
    return v.map(respText).whereType<String>().toList();
  }

  /// Number of keys in the store.
  int dbSize() => _want(cmd(['DBSIZE'])) as int;

  /// Remove every key.
  void flushAll() => _want(cmd(['FLUSHALL']));

  /// The boot-replay verdict: what this open restored — and what it could
  /// not. `droppedBytes > 0` or `corrupt` means the store recovered LESS
  /// than its files held (the dropped region was quarantined next to the
  /// AOF): surface it as a startup health check instead of scraping the
  /// boot WARN line from stderr.
  KevyOpenStats openReport() {
    final out = malloc<KevyOpenReport>();
    try {
      if (_b.kevy_open_report(_live, out) != 0) {
        throw KevyError('kevy: open_report failed');
      }
      final r = out.ref;
      return KevyOpenStats(
        replayedCommands: r.replayed_commands,
        replayedBytes: r.replayed_bytes,
        elapsedMs: r.elapsed_ms,
        droppedBytes: r.dropped_bytes,
        corrupt: r.corrupt != 0,
        quarantineCount: r.quarantine_count,
      );
    } finally {
      malloc.free(out);
    }
  }

  /// Publish [payload] to [channel]; returns the number of receivers.
  /// Scalar lane (kevy_publish): no argv packing, no RESP reply to allocate
  /// and parse — the publish analog of the direct subscribe symbol below.
  int publish(String channel, Uint8List payload) {
    final c = utf8.encode(channel);
    final buf = malloc<ffi.Uint8>(c.length + payload.length);
    try {
      buf.asTypedList(c.length + payload.length)
        ..setAll(0, c)
        ..setAll(c.length, payload);
      final n = _b.kevy_publish(
          _live, buf, c.length, buf + c.length, payload.length);
      if (n < 0) throw KevyError('kevy: publish failed');
      return n;
    } finally {
      malloc.free(buf);
    }
  }

  /// Subscribe to a channel (or a glob [pattern] when [pattern] is true).
  KevySub subscribe(String channel, {bool pattern = false}) {
    final c = utf8.encode(channel);
    final cp = malloc<ffi.Uint8>(c.length);
    try {
      cp.asTypedList(c.length).setAll(0, c);
      final sub = pattern
          ? _b.kevy_psubscribe(_live, cp, c.length)
          : _b.kevy_subscribe(_live, cp, c.length);
      if (sub == ffi.nullptr) throw KevyError('kevy: subscribe failed');
      return KevySub._(sub);
    } finally {
      malloc.free(cp);
    }
  }

  /// Close the store, releasing the native handle. Idempotent.
  void close() {
    if (_p != ffi.nullptr) {
      _dbFinalizer.detach(this);
      _b.kevy_close(_p);
    }
    _p = ffi.nullptr;
  }

  static Object? _want(Object? v) {
    if (v is KevyError) throw v;
    return v;
  }
}

// Copy a reply buffer into Dart bytes (parsed to a value), then free it.
Object? _takeBuf(KevyBuf buf) {
  if (buf.len == 0) return null;
  final bytes = Uint8List.fromList(buf.ptr.asTypedList(buf.len));
  _b.kevy_buf_free(buf.ptr, buf.len, buf.cap);
  return parseResp(bytes);
}

/// The boot-replay verdict [KevyDb.openReport] returns — the typed face of
/// the C ABI's KevyOpenReport.
class KevyOpenStats {
  const KevyOpenStats({
    required this.replayedCommands,
    required this.replayedBytes,
    required this.elapsedMs,
    required this.droppedBytes,
    required this.corrupt,
    required this.quarantineCount,
  });

  /// Commands replayed from the AOF(s), summed across shards.
  final int replayedCommands;

  /// Bytes actually replayed (the valid prefixes).
  final int replayedBytes;

  /// Wall-clock time of the startup replay, in milliseconds.
  final int elapsedMs;

  /// Bytes dropped past the last replayable frame (quarantined on disk).
  final int droppedBytes;

  /// True when any shard's replay stopped at a corrupt frame.
  final bool corrupt;

  /// Quarantine files written by the open's repair.
  final int quarantineCount;
}

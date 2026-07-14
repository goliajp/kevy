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
// (libkevy_ffi.so, dlopen by name); iOS links the static xcframework
// into the app binary, so its symbols are in the process image.
final FlutterKevyBindings _b = FlutterKevyBindings(() {
  if (Platform.isAndroid) return ffi.DynamicLibrary.open('libkevy_ffi.so');
  if (Platform.isIOS || Platform.isMacOS) return ffi.DynamicLibrary.process();
  if (Platform.isLinux) return ffi.DynamicLibrary.open('libkevy_ffi.so');
  if (Platform.isWindows) return ffi.DynamicLibrary.open('kevy_ffi.dll');
  throw UnsupportedError('kevy: unsupported platform');
}());

/// A polled pub/sub subscription. [next] drains one frame (or null when
/// the queue is empty); [close] unsubscribes.
class KevySub {
  ffi.Pointer<KevySubHandle> _p;
  KevySub._(this._p);

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

  void close() {
    if (_p != ffi.nullptr) _b.kevy_sub_close(_p);
    _p = ffi.nullptr;
  }
}

/// The embedded engine. One instance owns its data directory.
class KevyDb {
  ffi.Pointer<KevyDbHandle> _p;
  KevyDb._(this._p);

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

  void setText(String key, String value, {int ttlMs = 0}) =>
      set(key, utf8.encode(value), ttlMs: ttlMs);

  Uint8List? get(String key) {
    final k = utf8.encode(key);
    final kp = malloc<ffi.Uint8>(k.length);
    final out = calloc<KevyBuf>();
    try {
      kp.asTypedList(k.length).setAll(0, k);
      final rc = _b.kevy_get(_live, kp, k.length, out);
      if (rc < 0) throw KevyError('kevy: kevy_get misuse');
      if (rc == 0) return null;
      // The scalar fast path returns RAW value bytes, not a RESP reply —
      // take them directly, do NOT parse.
      final b = out.ref;
      final bytes = Uint8List.fromList(b.ptr.asTypedList(b.len));
      _b.kevy_buf_free(b.ptr, b.len, b.cap);
      return bytes;
    } finally {
      malloc.free(kp);
      calloc.free(out);
    }
  }

  String? getText(String key) {
    final v = get(key);
    return v == null ? null : utf8.decode(v);
  }

  // ── the typed surface (mirrors the other kevy packages) ─────────────
  int del(List<String> keys) => _want(cmd(['DEL', ...keys])) as int;

  int incrBy(String key, int delta) =>
      _want(cmd(['INCRBY', key, '$delta'])) as int;

  bool expire(String key, int ttlMs) =>
      _want(cmd(['PEXPIRE', key, '$ttlMs'])) == 1;

  int pttlMs(String key) => _want(cmd(['PTTL', key])) as int;

  List<String> keys([String pattern = '*']) {
    final v = _want(cmd(['KEYS', pattern]));
    if (v is! List) return [];
    return v.map(respText).whereType<String>().toList();
  }

  int dbSize() => _want(cmd(['DBSIZE'])) as int;

  void flushAll() => _want(cmd(['FLUSHALL']));

  int publish(String channel, Uint8List payload) =>
      _want(cmd(['PUBLISH', channel, payload])) as int;

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

  void close() {
    if (_p != ffi.nullptr) _b.kevy_close(_p);
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

// Host-side FFI test for the flutter_kevy door: drives the real kevy-ffi
// engine directly (open the cdylib, subscribe, publish, receive) so the
// generated bindings + the KevySub blocking path are exercised without a
// device. Locates the engine under the repo's target/ and skips cleanly
// when it hasn't been built (mirrors the python port's server discovery).
import 'dart:ffi' as ffi;
import 'dart:io';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';
import 'package:flutter_kevy/flutter_kevy_bindings_generated.dart';
import 'package:flutter_test/flutter_test.dart';

String? _findEngine() {
  // test/ → bindings/flutter → bindings → repo root.
  final root = Directory.current.path;
  final names = Platform.isMacOS
      ? ['libkevy_ffi.dylib']
      : Platform.isWindows
          ? ['kevy_ffi.dll']
          : ['libkevy_ffi.so'];
  for (final profile in ['debug', 'release']) {
    for (final n in names) {
      for (final base in ['$root/../../target', '$root/target']) {
        final p = '$base/$profile/$n';
        if (File(p).existsSync()) return p;
      }
    }
  }
  return null;
}

void main() {
  final enginePath = _findEngine();

  test('KevySub blocking wait (kevy_sub_wait) delivers a published frame', () {
    final b = FlutterKevyBindings(ffi.DynamicLibrary.open(enginePath!));
    final db = b.kevy_open_mem();
    expect(db, isNot(ffi.nullptr));

    final chan = 'ch'.toNativeUtf8();
    final sub = b.kevy_subscribe(db, chan.cast(), 'ch'.length);
    expect(sub, isNot(ffi.nullptr));

    _cmd(b, db, ['PUBLISH', 'ch', 'hi']);

    var got = false;
    for (var i = 0; i < 5 && !got; i++) {
      final out = calloc<KevyBuf>();
      final rc = b.kevy_sub_wait(sub, 500, out);
      expect(rc, greaterThanOrEqualTo(0));
      if (rc == 0) {
        calloc.free(out);
        break;
      }
      final bytes = _take(out.ref);
      calloc.free(out);
      if (String.fromCharCodes(bytes).contains('hi')) got = true;
    }

    b.kevy_sub_close(sub);
    b.kevy_close(db);
    calloc.free(chan);
    expect(got, isTrue, reason: 'waitNext should return the published frame');
  }, skip: enginePath == null ? 'kevy-ffi not built (run cargo build -p kevy-ffi)' : false);
}

Uint8List _take(KevyBuf buf) {
  if (buf.ptr == ffi.nullptr || buf.len == 0) return Uint8List(0);
  return Uint8List.fromList(buf.ptr.asTypedList(buf.len));
}

void _cmd(FlutterKevyBindings b, ffi.Pointer<KevyDbHandle> db, List<String> argv) {
  final argc = argv.length;
  final ptrs = calloc<ffi.Pointer<ffi.Uint8>>(argc);
  final lens = calloc<ffi.Size>(argc);
  for (var i = 0; i < argc; i++) {
    final bytes = Uint8List.fromList(argv[i].codeUnits);
    final p = calloc<ffi.Uint8>(bytes.length);
    p.asTypedList(bytes.length).setAll(0, bytes);
    ptrs[i] = p;
    lens[i] = bytes.length;
  }
  final out = calloc<KevyBuf>();
  final rc = b.kevy_cmd(db, argc, ptrs, lens, out);
  calloc.free(out);
  for (var i = 0; i < argc; i++) {
    calloc.free(ptrs[i]);
  }
  calloc.free(ptrs);
  calloc.free(lens);
  if (rc < 0) throw StateError('kevy_cmd misuse rc=$rc');
}

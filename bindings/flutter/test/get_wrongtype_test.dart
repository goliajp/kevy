// WRONGTYPE fidelity for the high-level get(): a GET on a non-string key
// must surface the engine's -WRONGTYPE as a typed KevyError, not collapse
// into an opaque "misuse". Drives the real KevyDb API host-side by pre-
// loading the kevy-ffi cdylib so DynamicLibrary.process() (what the library
// uses on macOS) can resolve its symbols; skips cleanly when it isn't built.
import 'dart:ffi' as ffi;
import 'dart:io';
import 'dart:typed_data';

import 'package:flutter_kevy/flutter_kevy.dart';
import 'package:flutter_test/flutter_test.dart';

String? _findEngine() {
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
  // Loading the cdylib publishes its symbols to the global/default handle the
  // library's DynamicLibrary.process() lookups (macOS) resolve against.
  if (enginePath != null) ffi.DynamicLibrary.open(enginePath);

  final skip = enginePath == null
      ? 'kevy-ffi not built (run cargo build -p kevy-ffi)'
      : false;

  test('get() on a list key surfaces a typed WRONGTYPE KevyError', () {
    final db = KevyDb.openInMemory();
    try {
      // Make the key hold a list, then GET it through the scalar lane.
      db.cmd(['RPUSH', 'mylist', 'a', 'b']);
      KevyError? caught;
      try {
        db.get('mylist');
      } on KevyError catch (e) {
        caught = e;
      }
      expect(caught, isNotNull,
          reason: 'GET on a list must throw, not return bytes');
      expect(caught!.message, contains('WRONGTYPE'));
    } finally {
      db.close();
    }
  }, skip: skip);

  test('get() still round-trips a string value and honours misses', () {
    final db = KevyDb.openInMemory();
    try {
      db.set('k', Uint8List.fromList([1, 2, 3]));
      expect(db.get('k'), equals(Uint8List.fromList([1, 2, 3])));
      expect(db.get('absent'), isNull);
      db.set('empty', Uint8List(0));
      expect(db.get('empty'), equals(Uint8List(0)));
    } finally {
      db.close();
    }
  }, skip: skip);
}

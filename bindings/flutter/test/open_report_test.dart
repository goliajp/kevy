// openReport() over the real Dart API: a clean in-memory open reports
// zeros (nothing dropped, nothing corrupt, nothing quarantined). The
// damage-path semantics are engine-tested (kevy-embedded); this pins the
// FFI plumbing and the typed mapping. Same cdylib preload dance as the
// sibling host tests.
import 'dart:ffi' as ffi;
import 'dart:io';

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
  if (enginePath != null) ffi.DynamicLibrary.open(enginePath);

  final skip = enginePath == null
      ? 'kevy-ffi not built (run cargo build -p kevy-ffi)'
      : false;

  test('openReport() is zeroed for a clean open', () {
    final db = KevyDb.openInMemory();
    try {
      final r = db.openReport();
      expect(r.droppedBytes, 0);
      expect(r.corrupt, false);
      expect(r.quarantineCount, 0);
    } finally {
      db.close();
    }
  }, skip: skip);
}

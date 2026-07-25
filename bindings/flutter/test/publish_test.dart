// The scalar publish lane through the real KevyDb API: publish() calls
// kevy_publish directly (no argv packing, no RESP reply), so assert the
// receiver count and that a polled subscriber actually gets the frame —
// count parity with the framed lane and end-to-end delivery in one test.
// Same host-side cdylib preload dance as get_wrongtype_test.dart.
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
  if (enginePath != null) ffi.DynamicLibrary.open(enginePath);

  final skip = enginePath == null
      ? 'kevy-ffi not built (run cargo build -p kevy-ffi)'
      : false;

  test('publish() counts receivers and the subscriber gets the payload', () {
    final db = KevyDb.openInMemory();
    try {
      // No subscribers yet: count 0.
      expect(db.publish('room', Uint8List.fromList('x'.codeUnits)), 0);

      final sub = db.subscribe('room');
      sub.next(); // drain the subscribe ack

      expect(db.publish('room', Uint8List.fromList('hello'.codeUnits)), 1);
      // next() parses the frame: [kind, channel, payload].
      final frame = sub.next()! as List<Object?>;
      expect(String.fromCharCodes(frame[2]! as List<int>), 'hello');

      // Empty payload is legal and still delivered.
      expect(db.publish('room', Uint8List(0)), 1);
      sub.close();
    } finally {
      db.close();
    }
  }, skip: skip);
}

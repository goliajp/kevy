// RESP decode for kevy replies — the Dart twin of the other doors'
// parsers. One complete reply in, a Dart value out:
//   +OK      -> String
//   -ERR …   -> KevyError (returned, not thrown — the engine saying no
//               is data; throwing is reserved for ABI misuse)
//   :42      -> int
//   $N …     -> Uint8List (call respText() for strings)
//   $-1/*-1  -> null
//   *N …     -> List
import 'dart:convert';
import 'dart:typed_data';

/// Thrown on ABI misuse and by the typed surface on a protocol error.
/// [KevyDb.cmd] returns it as a VALUE instead — driving the raw verb
/// surface, the engine saying no is data.
class KevyError {
  final String message;
  KevyError(this.message);
  @override
  String toString() => 'KevyError: $message';
}

class _Parsed {
  final Object? value;
  final int used;
  _Parsed(this.value, this.used);
}

/// Parse one complete RESP reply.
Object? parseResp(Uint8List buf) {
  final r = _one(buf, 0);
  if (r.used != buf.length) {
    throw StateError('kevy: trailing RESP bytes');
  }
  return r.value;
}

/// Bulk / simple payload as UTF-8 text, when that is what it is.
String? respText(Object? v) {
  if (v is Uint8List) return utf8.decode(v);
  if (v is String) return v;
  return null;
}

_Parsed _one(Uint8List b, int at) {
  final nl = _line(b, at + 1);
  final head = utf8.decode(b.sublist(at + 1, nl));
  final after = nl + 2;
  switch (b[at]) {
    case 0x2b: // +
      return _Parsed(head, after);
    case 0x2d: // -
      return _Parsed(KevyError(head), after);
    case 0x3a: // :
      return _Parsed(int.parse(head), after);
    case 0x24: // $
      final n = int.parse(head);
      if (n < 0) return _Parsed(null, after);
      // sublist already returns a fresh Uint8List — no fromList copy needed.
      return _Parsed(b.sublist(after, after + n), after + n + 2);
    case 0x2a: // *
      final n = int.parse(head);
      if (n < 0) return _Parsed(null, after);
      final items = <Object?>[];
      var pos = after;
      for (var i = 0; i < n; i++) {
        final r = _one(b, pos);
        items.add(r.value);
        pos = r.used;
      }
      return _Parsed(items, pos);
    default:
      throw StateError('kevy: unknown RESP tag ${b[at]}');
  }
}

int _line(Uint8List b, int from) {
  for (var i = from; i + 1 < b.length; i++) {
    if (b[i] == 13 && b[i + 1] == 10) return i;
  }
  throw StateError('kevy: truncated RESP reply');
}

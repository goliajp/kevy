// flutter_kevy example — the on-device smoke, rendered. Every assertion
// mobilegate drives headlessly runs here on start and paints green or
// red, and logs MOBILEGATE:<PASS|FAIL> so the gate can read the verdict
// from the device log. Synchronous kevy calls, the MMKV shape.

import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_kevy/flutter_kevy.dart';
import 'package:path_provider/path_provider.dart';

void main() {
  runApp(const MyApp());
}

class _Line {
  final String label;
  final bool ok;
  final String got;
  _Line(this.label, this.ok, this.got);
}

class MyApp extends StatefulWidget {
  const MyApp({super.key});
  @override
  State<MyApp> createState() => _MyAppState();
}

class _MyAppState extends State<MyApp> {
  List<_Line> _lines = [];
  String? _err;

  @override
  void initState() {
    super.initState();
    _run();
  }

  Future<void> _run() async {
    final lines = <_Line>[];
    void check(String label, bool ok, [Object? got = '']) =>
        lines.add(_Line(label, ok, '$got'));
    try {
      final docs = await getApplicationDocumentsDirectory();
      final dir = '${docs.path}/kevy-smoke';
      // A fresh dir each run keeps the smoke deterministic.
      final d = Directory(dir);
      if (d.existsSync()) d.deleteSync(recursive: true);

      var db = KevyDb.open(dir);
      db.setText('k', 'v');
      check('SET/GET', db.getText('k') == 'v', db.getText('k') ?? '∅');

      db.setText('tmp', 'x', ttlMs: 30000);
      final ttl = db.pttlMs('tmp');
      check('TTL', ttl > 0 && ttl <= 30000, '${ttl}ms');

      check('INCRBY', db.incrBy('n', 5) == 5, db.incrBy('n', 0));
      db.close();

      // Durability: reopen the same dir, the key survived.
      db = KevyDb.open(dir);
      check('REOPEN', db.getText('k') == 'v', db.getText('k') ?? '∅');
      db.flushAll();
      db.close();

      check('VERSION', RegExp(r'^\d+\.\d+\.\d+$').hasMatch(KevyDb.version()),
          KevyDb.version());

      final allOk = lines.every((l) => l.ok);
      print(allOk ? 'MOBILEGATE:PASS' : 'MOBILEGATE:FAIL');
      setState(() => _lines = lines);
    } catch (e) {
      print('MOBILEGATE:ERROR $e');
      setState(() => _err = '$e');
    }
  }

  @override
  Widget build(BuildContext context) {
    final allOk = _lines.isNotEmpty && _lines.every((l) => l.ok) && _err == null;
    return MaterialApp(
      home: Scaffold(
        appBar: AppBar(title: const Text('flutter_kevy smoke')),
        body: Padding(
          padding: const EdgeInsets.all(24),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(
                _err != null
                    ? 'ERROR'
                    : allOk
                        ? 'ALL PASS'
                        : 'running…',
                style: TextStyle(
                    fontSize: 22,
                    fontWeight: FontWeight.bold,
                    color: allOk ? Colors.green : Colors.red),
              ),
              if (_err != null) Text(_err!),
              const SizedBox(height: 12),
              for (final l in _lines)
                Row(children: [
                  Text(l.ok ? '✓ ' : '✗ ',
                      style:
                          TextStyle(color: l.ok ? Colors.green : Colors.red)),
                  SizedBox(width: 110, child: Text(l.label)),
                  Expanded(
                      child: Text(l.got,
                          style: const TextStyle(color: Colors.grey))),
                ]),
            ],
          ),
        ),
      ),
    );
  }
}

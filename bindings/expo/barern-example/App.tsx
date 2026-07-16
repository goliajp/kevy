// expo-kevy in a BARE React Native app — the same on-device smoke the Expo
// example runs, proving the module works via expo-modules-core autolinking
// in a plain @react-native-community/cli project (no Expo prebuild). Every
// assertion the mobilegate drives runs on mount and paints green or red;
// the verdict line MOBILEGATE:PASS/FAIL is what the gate reads from logcat.
// Synchronous kevy calls on the JS thread — the MMKV shape.
import { useEffect, useState } from 'react';
import { ScrollView, StyleSheet, Text, View } from 'react-native';
import { documentDirectory } from 'expo-file-system/legacy';
import { open, version } from 'expo-kevy';

type Line = { label: string; ok: boolean; got: string };

function runSmoke(): Line[] {
  const out: Line[] = [];
  const check = (label: string, ok: boolean, got: unknown = '') =>
    out.push({ label, ok, got: String(got) });

  // documentDirectory is a file:// URI on device; open() accepts it.
  const dir = `${documentDirectory}kevy-smoke`;
  let db = open({ dir });

  db.set('k', 'v');
  check('SET/GET', db.getText('k') === 'v', db.getText('k') ?? '∅');

  db.set('tmp', 'x', { ttlMs: 30_000 });
  const ttl = db.pttl('tmp');
  check('TTL', ttl > 0 && ttl <= 30_000, `${ttl}ms`);

  check('INCRBY', db.incrby('n', 5) === 5, db.incrby('n', 0));

  // WRONGTYPE fidelity: GET on a list key must throw a typed WRONGTYPE, not
  // a phantom miss — exercises the scalar -2 → ScalarGetSignal → Kotlin
  // catch → TS framed-fallback chain, and catches a stale vendored native.
  db.cmd('LPUSH', 'lst', 'x');
  let wt = false;
  let wtGot = 'no-throw (phantom miss — stale native?)';
  try {
    db.get('lst');
  } catch (e) {
    wtGot = String(e);
    wt = /WRONGTYPE/i.test(wtGot);
  }
  check('WRONGTYPE', wt, wtGot);

  db.close();

  // Durability: reopen the same dir, the key survived the close.
  db = open({ dir });
  check('REOPEN', db.getText('k') === 'v', db.getText('k') ?? '∅');
  db.flushall();
  db.close();

  check('VERSION', /^\d+\.\d+\.\d+$/.test(version()), version());
  return out;
}

function App() {
  const [lines, setLines] = useState<Line[]>([]);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    try {
      const result = runSmoke();
      setLines(result);
      // The side-channel mobilegate reads: one line, once settled.
      console.log(
        result.every(l => l.ok) ? 'MOBILEGATE:PASS' : 'MOBILEGATE:FAIL',
      );
    } catch (e) {
      setErr(String(e));
      console.log(`MOBILEGATE:ERROR ${String(e)}`);
    }
  }, []);

  const allOk = lines.length > 0 && lines.every(l => l.ok) && !err;

  return (
    <ScrollView contentContainerStyle={styles.page}>
      <Text style={styles.title}>expo-kevy smoke (bare RN)</Text>
      <Text style={[styles.verdict, { color: allOk ? '#1a7f37' : '#cf222e' }]}>
        {err ? 'ERROR' : allOk ? 'ALL PASS' : 'running…'}
      </Text>
      {err ? <Text style={styles.err}>{err}</Text> : null}
      {lines.map(l => (
        <View key={l.label} style={styles.row}>
          <Text style={styles.mark}>{l.ok ? '✓' : '✗'}</Text>
          <Text style={styles.label}>{l.label}</Text>
          <Text style={styles.got}>{l.got}</Text>
        </View>
      ))}
    </ScrollView>
  );
}

const styles = StyleSheet.create({
  page: { padding: 48, gap: 8 },
  title: { fontSize: 22, fontWeight: '600' },
  verdict: { fontSize: 18, fontWeight: '700', marginBottom: 12 },
  err: { color: '#cf222e', fontFamily: 'Courier' },
  row: { flexDirection: 'row', alignItems: 'center', gap: 10 },
  mark: { fontSize: 16, width: 18 },
  label: { fontSize: 16, width: 110 },
  got: { fontSize: 14, color: '#57606a', flexShrink: 1 },
});

export default App;

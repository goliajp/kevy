// expo-kevy example — the on-device smoke, rendered. Every assertion the
// mobilegate drives headlessly runs here on mount and paints green or
// red, so the same checks are a human demo and a CI gate. Synchronous
// kevy calls on the JS thread, the MMKV shape.
import { useEffect, useState } from "react";
import { ScrollView, StyleSheet, Text, View } from "react-native";
import { documentDirectory } from "expo-file-system/legacy";
import { open, text, version } from "expo-kevy";
import { runPubsubBench, pubsubGateLines, type BenchRow } from "./pubsubBench";
import { runNitroBench } from "./nitroBench";

type Line = { label: string; ok: boolean; got: string };

function runSmoke(): Line[] {
  const out: Line[] = [];
  const check = (label: string, ok: boolean, got: unknown = "") =>
    out.push({ label, ok, got: String(got) });

  // documentDirectory is a file:// URI on device; open() accepts it.
  const dir = `${documentDirectory}kevy-smoke`;
  let db = open({ dir });

  db.set("k", "v");
  check("SET/GET", db.getText("k") === "v", db.getText("k") ?? "∅");

  db.set("tmp", "x", { ttlMs: 30_000 });
  const ttl = db.pttl("tmp");
  check("TTL", ttl > 0 && ttl <= 30_000, `${ttl}ms`);

  check("INCRBY", db.incrby("n", 5) === 5, db.incrby("n", 0));

  db.close();

  // Durability: reopen the same dir, the key survived the close.
  db = open({ dir });
  check("REOPEN", db.getText("k") === "v", db.getText("k") ?? "∅");
  db.flushall();
  db.close();

  check("VERSION", /^\d+\.\d+\.\d+$/.test(version()), version());
  return out;
}

export default function App() {
  const [lines, setLines] = useState<Line[]>([]);
  const [bench, setBench] = useState<BenchRow[]>([]);
  const [nitro, setNitro] = useState<string[]>([]);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    try {
      const result = runSmoke();
      setLines(result);
      // The side-channel mobilegate reads: one line, once settled.
      console.log(result.every((l) => l.ok) ? "MOBILEGATE:PASS" : "MOBILEGATE:FAIL");
    } catch (e) {
      setErr(String(e));
      console.log(`MOBILEGATE:ERROR ${String(e)}`);
    }
    // pub/sub throughput vs mitt — pubsubgate reads the PUBSUBGATE lines.
    try {
      const rows = runPubsubBench();
      setBench(rows);
      for (const line of pubsubGateLines(rows)) console.log(line);
    } catch (e) {
      console.log(`PUBSUBGATE:ERROR ${String(e)}`);
    }
    // Nitro (JSI) fast-path door vs the Expo door — nitrogate reads the
    // NITROGATE lines (cmd round-trips through both doors).
    try {
      const nitroLines = runNitroBench();
      setNitro(nitroLines);
      for (const line of nitroLines) console.log(line);
    } catch (e) {
      console.log(`NITROGATE:ERROR ${String(e)}`);
    }
  }, []);

  const allOk = lines.length > 0 && lines.every((l) => l.ok) && !err;

  return (
    <ScrollView contentContainerStyle={styles.page}>
      <Text style={styles.title}>expo-kevy smoke</Text>
      <Text style={[styles.verdict, { color: allOk ? "#1a7f37" : "#cf222e" }]}>
        {err ? "ERROR" : allOk ? "ALL PASS" : "running…"}
      </Text>
      {err ? <Text style={styles.err}>{err}</Text> : null}
      {lines.map((l) => (
        <View key={l.label} style={styles.row}>
          <Text style={styles.mark}>{l.ok ? "✓" : "✗"}</Text>
          <Text style={styles.label}>{l.label}</Text>
          <Text style={styles.got}>{l.got}</Text>
        </View>
      ))}
      {bench.length > 0 ? (
        <Text style={[styles.title, { fontSize: 16, marginTop: 16 }]}>
          pub/sub throughput (ops/s)
        </Text>
      ) : null}
      {bench.map((b) => (
        <View key={b.label} style={styles.row}>
          <Text style={styles.label}>{b.label}</Text>
          <Text style={styles.got}>
            {b.opsPerSec.toLocaleString()} ({b.detail})
          </Text>
        </View>
      ))}
      {nitro.length > 0 ? (
        <Text style={[styles.title, { fontSize: 16, marginTop: 16 }]}>
          Nitro (JSI) door vs Expo door
        </Text>
      ) : null}
      {nitro.map((l) => (
        <Text key={l} style={styles.got}>
          {l.replace(/^NITROGATE:\s*/, "")}
        </Text>
      ))}
    </ScrollView>
  );
}

const styles = StyleSheet.create({
  page: { padding: 48, gap: 8 },
  title: { fontSize: 22, fontWeight: "600" },
  verdict: { fontSize: 18, fontWeight: "700", marginBottom: 12 },
  err: { color: "#cf222e", fontFamily: "Courier" },
  row: { flexDirection: "row", alignItems: "center", gap: 10 },
  mark: { fontSize: 16, width: 18 },
  label: { fontSize: 16, width: 110 },
  got: { fontSize: 14, color: "#57606a", flexShrink: 1 },
});

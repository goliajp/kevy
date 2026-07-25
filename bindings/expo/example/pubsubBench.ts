// pub/sub throughput: kevy's native pub/sub (across the JS↔native door)
// vs mitt (an in-process JS emitter — no serialization, no boundary). The
// point isn't to beat mitt on raw dispatch (mitt is a same-thread
// function call; kevy crosses JS↔native + RESP-frames + channel-matches).
// It's to *quantify the crossing tax* on device (Hermes), so we know how
// much a JSI/Nitro fast-path door would buy — and whether kevy pub/sub is
// already fast enough to serve as an event bus where you want its extra
// power (pattern subscribe, decoupled/multi subscribers, cross-context,
// and it doubles as your store).
import mitt from "mitt";
import { open } from "expo-kevy";

export type BenchRow = { label: string; opsPerSec: number; detail: string };

// One channel, N publishes of a fixed payload, drained by one subscriber.
function benchOne(n: number, payloadLen: number): BenchRow[] {
  const rows: BenchRow[] = [];
  const payload = "x".repeat(payloadLen);
  const payloadBytes = new TextEncoder().encode(payload);

  // mitt — in-process, synchronous handler, payload by reference.
  {
    const emitter = mitt<{ ev: string }>();
    let recv = 0;
    emitter.on("ev", () => {
      recv++;
    });
    const t = Date.now();
    for (let i = 0; i < n; i++) emitter.emit("ev", payload);
    const ms = Math.max(1, Date.now() - t);
    rows.push({
      label: `mitt ${payloadLen}B`,
      opsPerSec: Math.round((recv / ms) * 1000),
      detail: `${recv}/${n} in ${ms}ms`,
    });
  }

  // kevy — native engine, in-memory, publish then drain via the raw Sub
  // (self-pumped, no timer). Crosses the JS↔native door each call.
  {
    const db = open({}); // no dir => pure in-memory
    const sub = db.subscribeRaw("ev");
    let recv = 0;
    const t = Date.now();
    for (let i = 0; i < n; i++) {
      db.publish("ev", payloadBytes);
      for (let f = sub.next(); f !== undefined; f = sub.next()) recv++;
    }
    for (let f = sub.next(); f !== undefined; f = sub.next()) recv++;
    const ms = Math.max(1, Date.now() - t);
    rows.push({
      label: `kevy ${payloadLen}B`,
      opsPerSec: Math.round((recv / ms) * 1000),
      detail: `${recv}/${n} in ${ms}ms`,
    });
    sub.close();
    db.close();
  }
  return rows;
}

export function runPubsubBench(): BenchRow[] {
  const N = 50_000;
  const rows: BenchRow[] = [];
  for (const size of [16, 256]) rows.push(...benchOne(N, size));
  return rows;
}

// One parseable line per payload size for the pubsubgate side-channel.
export function pubsubGateLines(rows: BenchRow[]): string[] {
  const out: string[] = [];
  for (let i = 0; i < rows.length; i += 2) {
    const mittRow = rows[i];
    const kevyRow = rows[i + 1];
    const ratio = (mittRow.opsPerSec / Math.max(1, kevyRow.opsPerSec)).toFixed(1);
    out.push(
      `PUBSUBGATE: ${mittRow.label}=${mittRow.opsPerSec} ops/s | ` +
        `${kevyRow.label}=${kevyRow.opsPerSec} ops/s | mitt/kevy=${ratio}x`
    );
  }
  return out;
}

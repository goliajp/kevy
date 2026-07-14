// NITROGATE — the crossing tax, attacked. The current Expo door crosses
// JS<->native via an Expo-module dispatch + packed-argv marshalling + JNI
// (measured ~6-7us/crossing, ~50k pub/sub msg/s on device). The Nitro door
// calls a C++ HybridObject *directly via JSI* (~100-500ns/call expected),
// passing argv as an ArrayBuffer by reference. This bench times identical
// cmd round-trips through both doors so the delta is purely the crossing.
import mitt from "mitt";
import { open } from "expo-kevy";
import { createKevyNitro } from "react-native-kevy-nitro";

const enc = new TextEncoder();
const dec = new TextDecoder();

// The same packed form the JNI/N-API doors speak: u32-LE length prefix per
// arg, then the bytes. Returns an ArrayBuffer sized exactly to the payload
// so it can be handed to the Nitro cmd() by reference.
function packAB(args: (string | Uint8Array)[]): ArrayBuffer {
  const bufs = args.map((a) => (a instanceof Uint8Array ? a : enc.encode(a)));
  let total = 0;
  for (const b of bufs) total += 4 + b.length;
  const out = new Uint8Array(total);
  const view = new DataView(out.buffer);
  let pos = 0;
  for (const b of bufs) {
    view.setUint32(pos, b.length, true);
    out.set(b, pos + 4);
    pos += 4 + b.length;
  }
  return out.buffer;
}

const ops = (n: number, ms: number) => Math.round((n / Math.max(1, ms)) * 1000);

export function runNitroBench(): string[] {
  const lines: string[] = [];
  const N = 100_000;

  const nitro = createKevyNitro();

  // Correctness smoke: the C++ door is really talking to the engine.
  const abi = nitro.abi();
  const pong = dec.decode(new Uint8Array(nitro.cmd(packAB(["PING"]))));
  lines.push(`NITROGATE:SMOKE abi=${abi} ping=${JSON.stringify(pong)}`);

  // Pure-JSI crossing ceiling: abi() does nothing but cross and return a
  // number — the floor cost of a Nitro call, no engine work.
  {
    const t = Date.now();
    for (let i = 0; i < N; i++) nitro.abi();
    lines.push(`NITROGATE: abi (pure-JSI) nitro=${ops(N, Date.now() - t)} ops/s`);
  }

  // cmd round-trip: PING (cheapest verb — isolates the crossing, not the
  // engine). Expo door vs Nitro door, same N, same verb.
  {
    const db = open({});
    let e = 0;
    {
      const t = Date.now();
      for (let i = 0; i < N; i++) db.cmd("PING");
      e = ops(N, Date.now() - t);
    }
    const ping = packAB(["PING"]);
    let nt = 0;
    {
      const t = Date.now();
      for (let i = 0; i < N; i++) nitro.cmd(ping);
      nt = ops(N, Date.now() - t);
    }
    lines.push(
      `NITROGATE: cmd PING expo=${e} ops/s | nitro=${nt} ops/s | nitro/expo=${(nt / Math.max(1, e)).toFixed(1)}x`
    );
    db.close();
  }

  // cmd round-trip: SET (a write with a small payload) — the real GET/SET
  // hot path a store binding lives on, both doors.
  {
    const db = open({});
    let e = 0;
    {
      const t = Date.now();
      for (let i = 0; i < N; i++) db.cmd("SET", "k", "v");
      e = ops(N, Date.now() - t);
    }
    const setcmd = packAB(["SET", "k", "v"]);
    let nt = 0;
    {
      const t = Date.now();
      for (let i = 0; i < N; i++) nitro.cmd(setcmd);
      nt = ops(N, Date.now() - t);
    }
    lines.push(
      `NITROGATE: cmd SET expo=${e} ops/s | nitro=${nt} ops/s | nitro/expo=${(nt / Math.max(1, e)).toFixed(1)}x`
    );
    db.close();
  }

  // Pub/sub (bonus): mitt (in-process, no boundary) vs the Nitro door
  // (publish + poll-drain, one crossing each), 16 B payload.
  {
    const M = 50_000;
    const payload = "x".repeat(16);
    let mittOps = 0;
    {
      const emitter = mitt<{ ev: string }>();
      let recv = 0;
      emitter.on("ev", () => recv++);
      const t = Date.now();
      for (let i = 0; i < M; i++) emitter.emit("ev", payload);
      mittOps = ops(recv, Date.now() - t);
    }
    let nitroOps = 0;
    let recv = 0;
    {
      const n2 = createKevyNitro();
      n2.subscribe("ev");
      const msg = enc.encode(payload).buffer;
      const t = Date.now();
      for (let i = 0; i < M; i++) {
        n2.publish("ev", msg);
        for (let f = n2.subNext(); f !== undefined; f = n2.subNext()) recv++;
      }
      for (let f = n2.subNext(); f !== undefined; f = n2.subNext()) recv++;
      nitroOps = ops(recv, Date.now() - t);
    }
    lines.push(
      `NITROGATE: pubsub 16B mitt=${mittOps} ops/s | nitro=${nitroOps} ops/s (${recv}/${M}) | mitt/nitro=${(mittOps / Math.max(1, nitroOps)).toFixed(1)}x`
    );
  }

  return lines;
}

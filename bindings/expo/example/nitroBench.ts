// NITROGATE — the crossing tax, attacked. The current Expo door crosses
// JS<->native via an Expo-module dispatch + packed-argv marshalling + JNI
// (measured ~6-7us/crossing, ~50k pub/sub msg/s on device). The Nitro door
// calls a C++ HybridObject *directly via JSI* (~100-500ns/call expected),
// passing argv as an ArrayBuffer by reference. This bench times identical
// cmd round-trips through both doors so the delta is purely the crossing.
import mitt from "mitt";
import { documentDirectory, deleteAsync } from "expo-file-system/legacy";
import { open } from "expo-kevy";
import {
  createKevyBus,
  createKevyNitro,
  createKevyNitroAt,
  unpackFrames,
  type KevyNitro,
} from "react-native-kevy-nitro";

// Held forever so the idle push subscription (and its native poller) is not
// GC'd — lets `adb top` sample the poller's idle CPU. See the tail of
// runNitroBench: with kevy_sub_wait the poller parks in the kernel, so this
// must read ~0% (vs the old yield-spin build burning ~one core).
let idleHold: KevyNitro | undefined;

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
const x = (a: number, b: number) => (a / Math.max(1, b)).toFixed(1);

export async function runNitroBench(): Promise<string[]> {
  const lines: string[] = [];
  const N = 100_000;

  const nitro = createKevyNitro();

  // Correctness smoke: the C++ door is really talking to the engine.
  const abi = nitro.abi();
  const pong = dec.decode(new Uint8Array(nitro.cmd(packAB(["PING"]))));
  lines.push(`NITROGATE:SMOKE abi=${abi} ping=${JSON.stringify(pong)}`);

  // Scheduler warmup: ~300 ms of the cheapest crossing BEFORE any timing, so
  // big-core promotion happens outside the measured windows. Measured on a
  // Samsung S22: cold runs sit on little cores at 1/5-1/6 the throughput of a
  // promoted run — every axis below moves together with placement, so warm up
  // once here rather than skew the first-timed axis.
  {
    const t0 = Date.now();
    while (Date.now() - t0 < 300) {
      for (let i = 0; i < 1_000; i++) nitro.abi();
    }
  }

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

  // Scalar KV door: getData/setData call kevy_get/kevy_set directly — no argv
  // packing, no RESP framing, no JS decode. Timed against the RESP cmd lane
  // (cmd GET / cmd SET) on the SAME Nitro door, so the ratio is purely the
  // RESP tax the scalar door removes. MMKV's device numbers (mmkvgate LEDGER)
  // are the external bar (~425 ns GET / ~565 ns SET, iOS sim).
  {
    const kv = createKevyNitro();
    const val16 = "v".repeat(16);
    const valAB = enc.encode(val16).buffer;
    kv.setData("k", valAB, 0); // seed so getData hits (string key, like MMKV)
    const getcmd = packAB(["GET", "k"]);
    const setcmd = packAB(["SET", "k", val16]);
    let cmdGet = 0, cmdSet = 0, sGet = 0, sSet = 0;
    { const t = Date.now(); for (let i = 0; i < N; i++) kv.cmd(getcmd); cmdGet = ops(N, Date.now() - t); }
    { const t = Date.now(); for (let i = 0; i < N; i++) kv.cmd(setcmd); cmdSet = ops(N, Date.now() - t); }
    { const t = Date.now(); for (let i = 0; i < N; i++) kv.getData("k"); sGet = ops(N, Date.now() - t); }
    { const t = Date.now(); for (let i = 0; i < N; i++) kv.setData("k", valAB, 0); sSet = ops(N, Date.now() - t); }
    lines.push(
      `NITROGATE: kv scalar getData=${sGet} setData=${sSet} ops/s | vs cmd=${x(sGet, cmdGet)}x get / ${x(sSet, cmdSet)}x set`
    );

    // Head-to-head vs MMKV across value sizes — the SAME get/set workload
    // through the kevy scalar door (getData/setData) and react-native-mmkv,
    // in one app on one device (real end-to-end, not KevyKit-direct like
    // mmkvgate). Larger values shift the cost from fixed per-op marshalling
    // toward the data copy: kevy reads from an in-memory hashmap + zero-copy
    // wraps the return, MMKV mmap-reads + protobuf-decodes — so the size
    // sweep shows whether kevy's engine edge surfaces once the binding layer
    // is competitive. MMKV is optional: a missing link degrades to a note.
    try {
      // Lazy so a missing/unlinked MMKV degrades to a note, not a bench crash.
      const { MMKV } = require("react-native-mmkv") as typeof import("react-native-mmkv");
      const m = new MMKV({ id: "nitrogate-mmkv" });
      // Durable kevy handle for the SET axis MMKV is implicitly playing on:
      // MMKV's mmap file IS its durability; the in-memory kv above is not a
      // fair opponent for writes. dir-open = AOF + appendfsync everysec —
      // buffered append, background fsync ≤1 s: comparable-to-stricter
      // durability vs MMKV's OS-writeback pages (bench/mmkvgate/LEDGER.md).
      // Fresh dir each run: no replay tax on open, no cross-run growth.
      const kvdDir = `${documentDirectory}kevy-nitrogate`;
      await deleteAsync(kvdDir, { idempotent: true }).catch(() => undefined);
      const kvd = createKevyNitroAt(kvdDir.replace(/^file:\/\//, ""));
      for (const size of [16, 256, 4096]) {
        const vAB = enc.encode("v".repeat(size)).buffer;
        kv.setData("k", vAB, 0);
        m.set("k", vAB);
        let kGet = 0, kSet = 0, dSet = 0, mGet = 0, mSet = 0;
        { const t = Date.now(); for (let i = 0; i < N; i++) kv.getData("k"); kGet = ops(N, Date.now() - t); }
        { const t = Date.now(); for (let i = 0; i < N; i++) kv.setData("k", vAB, 0); kSet = ops(N, Date.now() - t); }
        { const t = Date.now(); for (let i = 0; i < N; i++) kvd.setData("k", vAB, 0); dSet = ops(N, Date.now() - t); }
        { const t = Date.now(); for (let i = 0; i < N; i++) m.getBuffer("k"); mGet = ops(N, Date.now() - t); }
        { const t = Date.now(); for (let i = 0; i < N; i++) m.set("k", vAB); mSet = ops(N, Date.now() - t); }
        lines.push(
          `NITROGATE: kv-vs-mmkv ${size}B GET kevy=${kGet} mmkv=${mGet} | kevy/mmkv=${x(kGet, mGet)}x  SET kevy=${kSet} mmkv=${mSet} | kevy/mmkv=${x(kSet, mSet)}x`
        );
        lines.push(
          `NITROGATE: kv-vs-mmkv-durable ${size}B SET kevy=${dSet} mmkv=${mSet} | kevy/mmkv=${x(dSet, mSet)}x (kevy AOF everysec vs mmkv mmap)`
        );
      }
    } catch (e) {
      lines.push(`NITROGATE: kv-vs-mmkv SKIPPED (react-native-mmkv not linked: ${String(e)})`);
    }
  }

  // Pub/sub, four ways, 16 B payload, M messages each:
  //   mitt          — in-process JS emitter, no boundary (the floor).
  //   poll/drain    — publish + JS-side subNext loop (~3 crossings/msg).
  //   push/message  — native poller thread hops each frame to JS (1 hop/msg).
  //   push/batched  — poller drains all pending, one hop per batch.
  const M = 50_000;
  const payload = "x".repeat(16);
  const msg = enc.encode(payload).buffer;

  // mitt
  let mittOps = 0;
  {
    const emitter = mitt<{ ev: string }>();
    let recv = 0;
    emitter.on("ev", () => recv++);
    const t = Date.now();
    for (let i = 0; i < M; i++) emitter.emit("ev", payload);
    mittOps = ops(recv, Date.now() - t);
  }

  // poll/drain (the baseline this round attacks)
  let pollOps = 0;
  {
    const n2 = createKevyNitro();
    n2.subscribe("ev");
    let recv = 0;
    const t = Date.now();
    for (let i = 0; i < M; i++) {
      n2.publish("ev", msg);
      for (let f = n2.subNext(); f !== undefined; f = n2.subNext()) recv++;
    }
    for (let f = n2.subNext(); f !== undefined; f = n2.subNext()) recv++;
    pollOps = ops(recv, Date.now() - t);
  }

  // push/message — one native->JS hop per frame. Publish all M, then await
  // the callbacks draining on the JS thread; time the whole pipeline.
  let pushOps = 0;
  let pushRecv = 0;
  {
    const n3 = createKevyNitro();
    let resolveDone!: () => void;
    const done = new Promise<void>((r) => (resolveDone = r));
    n3.subscribePush("ev", () => {
      if (++pushRecv >= M) resolveDone();
    });
    const t = Date.now();
    for (let i = 0; i < M; i++) n3.publish("ev", msg);
    await done;
    pushOps = ops(pushRecv, Date.now() - t);
    n3.stopPush();
  }

  // push/batched — one hop per drained batch. hops reveals the batch factor.
  let batchedOps = 0;
  let batchedRecv = 0;
  let hops = 0;
  {
    const n4 = createKevyNitro();
    let resolveDone!: () => void;
    const done = new Promise<void>((r) => (resolveDone = r));
    n4.subscribePushBatched("ev", (packed, count) => {
      hops++;
      // One ArrayBuffer per batch now (not M) — slice zero-copy views back
      // out by walking the u32-LE length prefixes.
      const frames = unpackFrames(packed, count);
      batchedRecv += frames.length;
      if (batchedRecv >= M) resolveDone();
    });
    const t = Date.now();
    for (let i = 0; i < M; i++) n4.publish("ev", msg);
    await done;
    batchedOps = ops(batchedRecv, Date.now() - t);
    n4.stopPush();
  }

  // bare publish floor — no subscribers anywhere, no bus machinery: the
  // native publish leg alone (raw JSI dispatch + kevy_publish on an empty
  // bus). bus-vs-this splits the hybrid lane's cost into native vs JS parts.
  let pubFloorOps = 0;
  {
    const n6 = createKevyNitro();
    const t = Date.now();
    for (let i = 0; i < M; i++) n6.publish("ev", msg);
    pubFloorOps = ops(M, Date.now() - t);
  }

  // bus (hybrid) — the TS local-fanout lane: subscribers dispatch in JS
  // (mitt's physical position), each publish still crosses once to the
  // engine bus (raw-lane consumers + receiver count). The delivery leg pays
  // zero crossings; the publish leg pays exactly one.
  let busOps = 0;
  let busCount = 0;
  {
    const n5 = createKevyNitro();
    const bus = createKevyBus(n5);
    let recv = 0;
    bus.subscribe("ev", () => {
      recv++;
    });
    const t = Date.now();
    for (let i = 0; i < M; i++) busCount = bus.publish("ev", msg);
    // The M dispatch microtasks queued ahead of this continuation all run
    // before the await resumes — recv is final here.
    await Promise.resolve();
    busOps = ops(recv, Date.now() - t);
  }

  lines.push(
    `NITROGATE: pubsub 16B mitt=${mittOps} | bus=${busOps} | pubFloor=${pubFloorOps} | poll=${pollOps} | push=${pushOps} | pushBatched=${batchedOps} ops/s`
  );
  lines.push(
    `NITROGATE: pubsub push/poll=${x(pushOps, pollOps)}x | pushBatched/poll=${x(batchedOps, pollOps)}x | mitt/pushBatched=${x(mittOps, batchedOps)}x | mitt/bus=${x(mittOps, busOps)}x (bus count=${busCount})`
  );
  lines.push(
    `NITROGATE: pubsub detail push(${pushRecv}/${M}) batched(${batchedRecv}/${M} in ${hops} hops, avg ${Math.round(batchedRecv / Math.max(1, hops))}/hop)`
  );

  // Idle-CPU probe: hold one live push subscription on a channel nothing
  // ever publishes to. The native poller sits blocked in kevy_sub_wait
  // (kernel park) — sample the app's CPU now; it must be ~0%.
  idleHold = createKevyNitro();
  idleHold.subscribePush("idle-never-published", () => {});
  lines.push("NITROGATE:IDLE live push sub, no traffic — sample app CPU now");

  return lines;
}

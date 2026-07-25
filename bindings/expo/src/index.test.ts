// Conformance smoke for the expo door's TypeScript surface. The real native
// module needs an RN/Expo host, so we mock expo-modules-core with a fake that
// stands in for the two native shells: a scalar lane whose get() throws the
// KEVY_WRONGTYPE signal on a non-string key (as ios/KevyExpoModule.swift and
// android/.../KevyExpoModule.kt do), and a cmd() that speaks RESP. This locks
// the one behaviour every door must agree on: GET on a non-string key surfaces
// a typed WRONGTYPE, not an opaque throw.
import { beforeAll, expect, mock, test } from "bun:test";

const encoder = new TextEncoder();
const decoder = new TextDecoder();
const store = new Map<string, Uint8Array>();
const kinds = new Map<string, "string" | "list">();

// The fake native module — the JSI shell's shape, one level below src/index.ts.
const fakeNative = {
  open: (_dir: string | null) => 1,
  close: (_id: number) => {},
  version: () => "test",
  set: (_id: number, key: Uint8Array, value: Uint8Array, _ttlMs: number) => {
    const k = decoder.decode(key);
    store.set(k, value);
    kinds.set(k, "string");
  },
  get: (_id: number, key: Uint8Array): Uint8Array | null => {
    const k = decoder.decode(key);
    if (kinds.get(k) === "list") {
      const e = new Error("KEVY_WRONGTYPE: scalar GET on a non-string key") as Error & { code?: string };
      e.code = "ERR_KEVY_WRONGTYPE";
      throw e;
    }
    return store.get(k) ?? null;
  },
  cmd: (_id: number, packed: Uint8Array): Uint8Array => {
    // Decode argv (u32-LE length prefix per arg) to route the framed fallback.
    const view = new DataView(packed.buffer, packed.byteOffset, packed.byteLength);
    const argv: string[] = [];
    let pos = 0;
    while (pos + 4 <= packed.length) {
      const n = view.getUint32(pos, true);
      pos += 4;
      argv.push(decoder.decode(packed.subarray(pos, pos + n)));
      pos += n;
    }
    if (argv[0] === "GET" && kinds.get(argv[1]) === "list") {
      return encoder.encode("-WRONGTYPE Operation against a key holding the wrong kind of value\r\n");
    }
    return encoder.encode("$-1\r\n");
  },
  subscribe: (_id: number, _chan: Uint8Array, _pattern: boolean) => 1,
  subNext: (_subId: number): Uint8Array | null => null,
  subClose: (_subId: number) => {},
};

// Register a list key straight into the fake store (no native LPUSH needed).
function seedList(key: string): void {
  kinds.set(key, "list");
}

let open: typeof import("./index.ts").open;

beforeAll(async () => {
  mock.module("expo-modules-core", () => ({ requireNativeModule: () => fakeNative }));
  ({ open } = await import("./index.ts"));
});

test("scalar get/set round-trip and miss", () => {
  const db = open();
  db.set("k", "v");
  expect(db.getText("k")).toBe("v");
  expect(db.get("absent")).toBeNull();
  expect(db.getText("absent")).toBeUndefined();
  db.close();
});

test("GET on a non-string key surfaces a typed WRONGTYPE, not an opaque throw", () => {
  const db = open();
  seedList("mylist");
  expect(() => db.get("mylist")).toThrow(/WRONGTYPE/);
  db.close();
});

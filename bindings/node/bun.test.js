// The Bun smoke — same contract as the C/Go ones. ffigate runs
// `bun test bindings/node` after `cargo build -p kevy-ffi`.
import { expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { KevyError, abi, open, text, version } from "./bun.js";

test("smoke", () => {
  expect(abi()).toBe(1);
  expect(version()).toMatch(/^\d+\.\d+\.\d+$/);

  const dir = join(mkdtempSync(join(tmpdir(), "kevy-bun-")), "data");

  let db = open({ dir });
  expect(db.cmd("SET", "smoke:bun", "v1")).toBe("OK");
  expect(text(db.cmd("GET", "smoke:bun"))).toBe("v1");
  expect(db.cmd("NOSUCHVERB")).toBeInstanceOf(KevyError);

  const sub = db.subscribe("c1");
  expect(sub.next()).not.toBeUndefined(); // subscribe ack
  expect(db.cmd("PUBLISH", "c1", "hello")).toBe(1);
  const frame = sub.next();
  expect(text(frame[0])).toBe("message");
  expect(text(frame[1])).toBe("c1");
  expect(text(frame[2])).toBe("hello");
  expect(sub.next()).toBeUndefined(); // drained
  sub.close();

  // Durability: close, reopen the same dir, the key survived.
  db.close();
  db = open({ dir });
  expect(text(db.cmd("GET", "smoke:bun"))).toBe("v1");
  expect(db.cmd("DEL", "smoke:bun")).toBe(1);
  db.close();
});

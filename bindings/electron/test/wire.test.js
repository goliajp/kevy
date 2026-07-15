// The wire contract is duplicated on purpose: bridge.js imports wire.js, but
// the sandboxed preload cannot require a local module, so preload.cjs inlines
// its own copy. This test is the guard that keeps the two copies identical —
// same channel names, and encode(main) → decode(preload) round-trips.
import assert from "node:assert/strict";
import { test } from "node:test";

import * as wire from "../wire.js";
import * as preload from "../preload.cjs";

test("CHANNELS are identical in wire.js and preload.cjs", () => {
  assert.deepEqual(preload.CHANNELS, wire.CHANNELS);
});

test("encodeReply (main) round-trips through decodeReply (both sides)", () => {
  const bytes = new Uint8Array([1, 2, 3]);
  const value = [
    "OK",
    42,
    9007199254740993n,
    bytes,
    null,
    [new wire.KevyError("WRONGTYPE nope"), "nested"],
  ];
  const onWire = wire.encodeReply(value);

  for (const decode of [wire.decodeReply, preload.decodeReply]) {
    const back = decode(onWire);
    assert.equal(back[0], "OK");
    assert.equal(back[1], 42);
    assert.equal(back[2], 9007199254740993n);
    assert.deepEqual(back[3], bytes);
    assert.equal(back[4], null);
    assert.equal(back[5][0].message, "WRONGTYPE nope");
    assert.equal(back[5][1], "nested");
  }
  // the preload reconstructs its OWN KevyError class from the marker
  assert.ok(preload.decodeReply(onWire)[5][0] instanceof preload.KevyError);
});

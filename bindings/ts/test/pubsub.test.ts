// Pub/sub conformance (contract §3.11 / §6): message + pattern round-trips on
// the named embedded bus AND remote; anonymous mem:// rejected; RESP3 push
// via hello3 (remote); read-timeout bounds a blocking recv. node --test + bun.

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { connect, Subscriber, UnsupportedError } from "../src/index.ts";
import { spawnServer, uniqueMemUrl, s, type Server } from "./harness.ts";

let server: Server;
before(async () => {
  server = await spawnServer();
});
after(() => server?.close());

function pubsubUrls(): Array<[string, string]> {
  // [name, url]; embedded uses a unique named bus, remote the shared server.
  return [
    ["embedded", uniqueMemUrl()],
    ["remote", server.url],
  ];
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

test("publish → subscriber receives Message (both backends)", async () => {
  for (const [, url] of pubsubUrls()) {
    const sub = await Subscriber.connectChannels(url, "news");
    const ack = await sub.recv();
    assert.equal(ack.kind, "subscribe");
    const pub = await connect(url);
    await sleep(50);
    await pub.publish("news", "hello");
    const [ch, payload] = await sub.recvMessage();
    assert.equal(s(ch), "news");
    assert.equal(s(payload), "hello");
    sub.close();
    pub.close();
  }
});

test("pattern subscribe receives Pmessage (both backends)", async () => {
  for (const [, url] of pubsubUrls()) {
    const sub = await Subscriber.connect(url);
    await sub.psubscribe("ne*");
    const ack = await sub.recv();
    assert.equal(ack.kind, "psubscribe");
    const pub = await connect(url);
    await sleep(50);
    await pub.publish("news", "world");
    const [ch, payload] = await sub.recvMessage();
    assert.equal(s(ch), "news");
    assert.equal(s(payload), "world");
    sub.close();
    pub.close();
  }
});

test("anonymous mem:// Subscriber → Unsupported", async () => {
  await assert.rejects(Subscriber.connect("mem://"), (e) => e instanceof UnsupportedError);
});

test("remote hello3 upgrades to RESP3 push; recv handles it", async () => {
  const sub = await Subscriber.connect(server.url);
  await sub.hello3();
  await sub.subscribe("rt");
  const ack = await sub.recv();
  assert.equal(ack.kind, "subscribe");
  const pub = await connect(server.url);
  await sleep(50);
  await pub.publish("rt", "v3");
  const [ch, payload] = await sub.recvMessage();
  assert.equal(s(ch), "rt");
  assert.equal(s(payload), "v3");
  sub.close();
  pub.close();
});

test("embedded hello3 → Unsupported", async () => {
  const sub = await Subscriber.connect(uniqueMemUrl());
  await assert.rejects(sub.hello3(), (e) => e instanceof UnsupportedError);
  sub.close();
});

test("read-timeout bounds a blocking recv (both backends)", async () => {
  for (const [, url] of pubsubUrls()) {
    const sub = await Subscriber.connectChannels(url, "quiet");
    await sub.recv(); // drain ack
    sub.setReadTimeout(80);
    const start = Date.now();
    await assert.rejects(sub.recv(), (e) => (e as { kind?: string }).kind === "timedOut");
    assert.ok(Date.now() - start < 2000, "timeout bounded the recv");
    sub.close();
  }
});

// clientgate: node-redis (the `redis` package) against a kevy server.
import { createClient } from "redis";

const url = `redis://127.0.0.1:${process.env.KEVY_PORT}`;
const c = createClient({ url });
await c.connect();

function check(ok, what) {
  if (!ok) {
    console.error(`FAIL ${what}`);
    process.exit(1);
  }
}

await c.flushAll();
check((await c.set("k", "v")) === "OK", "SET");
check((await c.get("k")) === "v", "GET");
check((await c.incrBy("n", 5)) === 5, "INCRBY");
await c.pExpire("k", 30_000);
const ttl = await c.pTTL("k");
check(ttl > 0 && ttl <= 30_000, "PTTL");

await c.hSet("h", { a: "1", b: "2" });
check((await c.hGetAll("h")).a === "1", "HGETALL");
await c.lPush("l", ["x", "y"]);
check((await c.lRange("l", 0, -1)).join(",") === "y,x", "LRANGE");
await c.zAdd("z", [{ score: 1, value: "one" }, { score: 2, value: "two" }]);
check((await c.zRange("z", 0, -1)).join(",") === "one,two", "ZRANGE");

// pub/sub round trip (dedicated subscriber connection, client idiom)
const sub = c.duplicate();
await sub.connect();
const got = new Promise((r) => sub.subscribe("room", (msg) => r(msg)));
await c.publish("room", "hi");
check((await got) === "hi", "pubsub");
await sub.destroy();

// the extended verb surface through the raw channel
const idx = await c.sendCommand(["IDX.LIST"]);
check(Array.isArray(idx), "IDX.LIST raw");

await c.destroy();
console.log("ok");

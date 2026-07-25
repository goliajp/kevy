// clientgate: ioredis against a kevy server.
import Redis from "ioredis";

const port = Number(process.env.KEVY_PORT);
const c = new Redis({ port, host: "127.0.0.1", lazyConnect: true });
await c.connect();

function check(ok, what) {
  if (!ok) {
    console.error(`FAIL ${what}`);
    process.exit(1);
  }
}

await c.flushall();
check((await c.set("k", "v")) === "OK", "SET");
check((await c.get("k")) === "v", "GET");
check((await c.incrby("n", 5)) === 5, "INCRBY");
await c.pexpire("k", 30_000);
const ttl = await c.pttl("k");
check(ttl > 0 && ttl <= 30_000, "PTTL");

await c.hset("h", { a: "1", b: "2" });
check((await c.hgetall("h")).a === "1", "HGETALL");
await c.lpush("l", "x", "y");
check((await c.lrange("l", 0, -1)).join(",") === "y,x", "LRANGE");
await c.zadd("z", 1, "one", 2, "two");
check((await c.zrange("z", 0, -1)).join(",") === "one,two", "ZRANGE");

// pub/sub round trip
const sub = new Redis({ port, host: "127.0.0.1" });
const got = new Promise((r) => {
  sub.on("message", (_ch, msg) => r(msg));
});
await sub.subscribe("room");
await c.publish("room", "hi");
check((await got) === "hi", "pubsub");
sub.disconnect();

// the extended verb surface through the raw channel
const idx = await c.call("IDX.LIST");
check(Array.isArray(idx), "IDX.LIST raw");

c.disconnect();
console.log("ok");

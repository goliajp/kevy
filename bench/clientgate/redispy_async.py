# clientgate: redis-py's asyncio client against a kevy server.
# Same ladder as redispy.py, driven through redis.asyncio — the async door
# ships in the same `redis` pip package, so no extra dependency.
import asyncio
import os
import sys

import redis.asyncio as redis


def check(ok, what):
    if not ok:
        print(f"FAIL {what}", file=sys.stderr)
        sys.exit(1)


async def main():
    c = redis.Redis(host="127.0.0.1", port=int(os.environ["KEVY_PORT"]))

    await c.flushall()
    check(await c.set("k", "v") is True, "SET")
    check(await c.get("k") == b"v", "GET")
    check(await c.incrby("n", 5) == 5, "INCRBY")
    check(await c.pexpire("k", 30_000) is True, "PEXPIRE")
    check(0 < await c.pttl("k") <= 30_000, "PTTL")

    await c.hset("h", mapping={"a": "1", "b": "2"})
    check((await c.hgetall("h"))[b"a"] == b"1", "HGETALL")
    await c.lpush("l", "x", "y")
    check(await c.lrange("l", 0, -1) == [b"y", b"x"], "LRANGE")
    await c.zadd("z", {"one": 1, "two": 2})
    check(await c.zrange("z", 0, -1) == [b"one", b"two"], "ZRANGE")

    # pub/sub round trip
    p = c.pubsub(ignore_subscribe_messages=True)
    await p.subscribe("room")
    await c.publish("room", "hi")
    msg = await p.get_message(timeout=5)
    while msg is None:
        msg = await p.get_message(timeout=5)
    check(msg["data"] == b"hi", "pubsub")
    await p.aclose()

    # the extended verb surface through the raw channel
    idx = await c.execute_command("IDX.LIST")
    check(isinstance(idx, list), "IDX.LIST raw")

    await c.aclose()
    print("ok")


asyncio.run(main())

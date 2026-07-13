# clientgate: redis-py against a kevy server.
import os
import sys

import redis


def check(ok, what):
    if not ok:
        print(f"FAIL {what}", file=sys.stderr)
        sys.exit(1)


c = redis.Redis(host="127.0.0.1", port=int(os.environ["KEVY_PORT"]))

c.flushall()
check(c.set("k", "v") is True, "SET")
check(c.get("k") == b"v", "GET")
check(c.incrby("n", 5) == 5, "INCRBY")
check(c.pexpire("k", 30_000) is True, "PEXPIRE")
check(0 < c.pttl("k") <= 30_000, "PTTL")

c.hset("h", mapping={"a": "1", "b": "2"})
check(c.hgetall("h")[b"a"] == b"1", "HGETALL")
c.lpush("l", "x", "y")
check(c.lrange("l", 0, -1) == [b"y", b"x"], "LRANGE")
c.zadd("z", {"one": 1, "two": 2})
check(c.zrange("z", 0, -1) == [b"one", b"two"], "ZRANGE")

# pub/sub round trip
p = c.pubsub(ignore_subscribe_messages=True)
p.subscribe("room")
c.publish("room", "hi")
msg = p.get_message(timeout=5)
while msg is None:
    msg = p.get_message(timeout=5)
check(msg["data"] == b"hi", "pubsub")
p.close()

# the extended verb surface through the raw channel
idx = c.execute_command("IDX.LIST")
check(isinstance(idx, list), "IDX.LIST raw")

print("ok")

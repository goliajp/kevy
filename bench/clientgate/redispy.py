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
# Read the server's subscribe confirmation BEFORE publishing. redis-py's
# subscribe() only writes the command, so publishing straight after it is a
# race: the message goes to whoever is registered at PUBLISH time, and a lost
# message leaves the get_message loop below spinning forever. Single-threaded
# Redis survives it because arrival order is processing order; a
# thread-per-core server with the two connections on different shards does
# not. go-redis and hiredis already wait for this ack explicitly.
p = c.pubsub()
p.subscribe("room")
ack = p.get_message(timeout=5)
check(ack is not None and ack["type"] == "subscribe", "subscribe ack")
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

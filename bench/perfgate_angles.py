"""perfgate's angles: what each one starts, warms and loads.

An angle is one workload shape. The server runs SHARDS threads on the
server cpus; the load generators run on the load cpus and are killed once
the windows close, so their request count only has to outlast them.

  pinned_*   one generator per shard on that shard's own cluster port, keys
             hashtagged to the shard: nothing is forwarded
  compat_*   the same generators on the shared port, which spreads the
             connections over the shards in turn
  xshard_set untagged keys on the shared port: with n shards, (n-1)/n of
             the commands belong to another shard
  plain_*    the default deployment: no --cluster, so keys route by the
             keyspace hash; untagged keys on the shared port
  onekey_*   redis-benchmark -t, one fixed key: one shard does all the work
  zinterstore  a two-set ZINTERSTORE, on a one-shard server
  hybrid_p95 IDX.QUERY HYBRID p95 from one closed-loop client (latency only)
"""

import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
N_GEN = 2_000_000_000
# Between starting one load generator and the next: long enough for its
# connections to be made, so they are dealt before the next one's.
CONNECT_GAP_SECS = 0.3
N_HYBRID = 20_000

PINNED = {
    "get": "GET {T}:__rand_int__",
    "set": "SET {T}:__rand_int__ v",
    "incr": "INCR {T}:c",
    "sadd": "SADD {T}:s __rand_int__",
    "hset": "HSET {T}:h __rand_int__ v",
    "lpush": "LPUSH {T}:l v",
    "zadd": "ZADD {T}:z __rand_int__ m__rand_int__",
}

PLAIN = {"get": "GET key:__rand_int__", "set": "SET key:__rand_int__ v"}

ANGLES = ([f"pinned_{v}" for v in PINNED] + ["compat_get", "compat_set", "xshard_set"]
          + [f"plain_{v}" for v in PLAIN]
          + ["onekey_get", "onekey_set", "zinterstore", "hybrid_p95"])

# single-shard shapes for the callgrind mode: (warm command or None, load command)
CALLGRIND = {
    "get": ("SET k:__rand_int__ v", "GET k:__rand_int__"),
    "set": (None, "SET k:__rand_int__ v"),
    "incr": (None, "INCR c"),
    "sadd": (None, "SADD s __rand_int__"),
    "hset": (None, "HSET h __rand_int__ v"),
    "lpush": (None, "LPUSH l v"),
    "zadd": (None, "ZADD z __rand_int__ m__rand_int__"),
    # two shards: half the commands are forwarded, so the cross-shard
    # request and reply path is counted. The idle loop's instructions vary
    # with scheduling, so read these per function, not by their total.
    "xget": ("SET k:__rand_int__ v", "GET k:__rand_int__"),
    "xset": (None, "SET k:__rand_int__ v"),
    # the instruction side of onekey_* and zinterstore, whose throughput
    # rounds disagree on instructions with identical code
    "xonekey_get": ("SET key:onekey v", "GET key:onekey"),
    "xonekey_set": (None, "SET key:onekey v"),
    "zinterstore": ("zinterstore", "ZINTERSTORE zalg:dst:__rand_int__ 2 zalg:a zalg:b"),
}
# angles whose single request costs far more than a GET count fewer
CALLGRIND_OPS = {"zinterstore": 5_000}
# connections per angle (default 4); KEVY_CG_CONNS overrides, for experiments
CALLGRIND_CONNS = {}


def crc16(data):
    """CRC16-XMODEM, the cluster key-slot hash."""
    crc = 0
    for b in data:
        crc ^= b << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x1021) if crc & 0x8000 else crc << 1
            crc &= 0xFFFF
    return crc


def shard_tags(n):
    """A hashtag per shard: shard i of n owns slots with slot * n >> 14 == i."""
    tags, i = [None] * n, 0
    while None in tags:
        t = f"t{i}"
        s = ((crc16(t.encode()) % 16384) * n) >> 14
        if tags[s] is None:
            tags[s] = t
        i += 1
    return tags


def bench(port, *args, n=None, threads=None, conns=None, pipe=None, keyspace=None):
    argv = ["redis-benchmark", "-h", "127.0.0.1", "-p", str(port), "-q"]
    for flag, v in (("-n", n), ("--threads", threads), ("-c", conns), ("-P", pipe),
                    ("-r", keyspace)):
        if v is not None:
            argv += [flag, str(v)]
    return argv + list(args)


def run_quiet(argv):
    subprocess.run(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)


def cluster(angle):
    return angle.startswith(("pinned_", "compat_", "xshard_"))


def topology(angle, topo):
    """The server shape the angle runs on. ZINTERSTORE works on the one
    shard that owns both sets; on four, the other three idle and their idle
    loop is billed to every command, so it runs on one shard."""
    if angle != "zinterstore":
        return topo
    first = topo["srv_list"][0]
    return dict(topo, srv_threads=1, srv_cpus=str(first), srv_list=[first])


def warm(angle, port, shards):
    """Fill what the angle reads before its load starts."""
    if angle in ("pinned_get", "compat_get"):
        procs = [subprocess.Popen(bench(port + 1 + i, "SET", f"{{{t}}}:__rand_int__", "v",
                                        n=1_000_000, keyspace=1_000_000, pipe=64),
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                 for i, t in enumerate(shard_tags(shards))]
        for p in procs:
            p.wait()
    elif angle == "plain_get":
        run_quiet(bench(port, "SET", "key:__rand_int__", "v", n=1_000_000, keyspace=1_000_000,
                        pipe=64))
    elif angle == "onekey_get":
        run_quiet(bench(port, "-t", "set", n=300_000, pipe=64))
    elif angle == "zinterstore":
        for key in ("zalg:a", "zalg:b"):
            for i in range(10):
                args = [x for j in range(i * 100, i * 100 + 100) for x in (str(j), f"m{j}")]
                run_quiet(["redis-cli", "-p", str(port), "ZADD", key] + args)
    elif angle == "hybrid_p95":
        subprocess.run([sys.executable, str(HERE / "perfgate_hybrid.py"), "load", str(port)],
                       stdout=subprocess.DEVNULL, check=True)


def generators(angle, port, shards, cli_threads):
    """[(argv, threads)] of the angle's load; empty for the latency angle."""
    per = max(1, cli_threads // shards)
    tags = shard_tags(shards)
    if angle.startswith(("pinned_", "compat_")):
        cmd = PINNED[angle.split("_", 1)[1]]
        return [(bench(port + 1 + i if angle.startswith("pinned_") else port,
                       *cmd.replace("{T}", "{" + t + "}").split(),
                       n=N_GEN, keyspace=1_000_000, conns=12, pipe=256, threads=per), per)
                for i, t in enumerate(tags)]
    if angle.startswith("plain_"):
        return [(bench(port, *PLAIN[angle.split("_", 1)[1]].split(), n=N_GEN,
                       keyspace=1_000_000, conns=12, pipe=256, threads=per), per) for _ in tags]
    if angle == "xshard_set":
        return [(bench(port, "SET", "key:__rand_int__", "v", n=N_GEN, keyspace=1_000_000,
                       conns=12, pipe=256, threads=per), per) for _ in tags]
    if angle.startswith("onekey_"):
        return [(bench(port, "-t", angle.split("_", 1)[1], n=N_GEN, conns=50, pipe=256,
                       threads=cli_threads), cli_threads)]
    if angle == "zinterstore":
        return [(bench(port, "ZINTERSTORE", "zalg:dst:__rand_int__", "2", "zalg:a", "zalg:b",
                       n=N_GEN, conns=50, pipe=16, threads=cli_threads), cli_threads)]
    return []


def latency_cmd(port):
    return [sys.executable, str(HERE / "perfgate_hybrid.py"), "run", str(port), str(N_HYBRID)]

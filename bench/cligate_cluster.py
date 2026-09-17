"""cligate's cluster fixture — cluster-enabled redis-servers in two groups.

A cluster case names a shape (`cluster: 3x1`), and every run of it, redis-cli's
and kevy-cli's, starts from that shape built fresh. What the cluster manager
prints depends on more than the topology: it lists nodes in the order the
entry node's table holds them, and that order follows the node ids. So the
nodes keep their ids across cases — a reset is CLUSTER RESET SOFT, which
forgets every other node and every slot but not the node's own name — and
the servers are started once per gate run, not per case.

Shapes, over the current group's ports 17000-17007 (the bus is port + 10000):

    empty   every node alone, no slots
    3       17000-17002 masters: 0-5460, 5461-10922, 10923-16383
    3x1     3, plus 17003 replicating 17000, 17004 17001, 17005 17002
    uneven  17000-17002 masters: 0-6000, 6001-12000, 12001-16383
    3+1     uneven, plus 17003 a master without slots

`legacy-` before a shape builds it on the legacy group instead, ports
17010-17017 running Redis 7.4.10: a version without atomic slot migration,
where the cluster manager moves slots with MIGRATE. `valkey-` builds it on
ports 17020-17027 running the pinned Valkey, for valkey-cli's own features.

Nodes outside the shape are reset and left alone, for create and add-node.
Node ids are fixed too: node 17000 is 1700017000...17000.
"""

import time

PORTS = list(range(17000, 17008))
LEGACY_PORTS = list(range(17010, 17018))
LEGACY_IMAGE = "redis:7.4.10"
VALKEY_PORTS = list(range(17020, 17028))
# By node index within a group: masters (index, first slot, last slot),
# replicas (index, master index).
SHAPES = {
    "empty": ([], []),
    "3": ([(0, 0, 5460), (1, 5461, 10922), (2, 10923, 16383)], []),
    "3x1": ([(0, 0, 5460), (1, 5461, 10922), (2, 10923, 16383)], [(3, 0), (4, 1), (5, 2)]),
    # Every master owns a different number of slots, so a rebalance sorts
    # them the same way whatever order a node's table lists them in.
    "uneven": ([(0, 0, 6000), (1, 6001, 12000), (2, 12001, 16383)], []),
    "3+1": ([(0, 0, 6000), (1, 6001, 12000), (2, 12001, 16383), (3, None, None)], []),
}
CONVERGE_S = 20

# Reset every node: replicas first (a reset replica becomes an empty master),
# then masters, which must hold no keys. The node timeout set-timeout may have
# changed is put back — short, because a node that was reset waits about one
# node timeout before it calls the cluster ok again.
RESET = r"""
set -e
rc() { redis-cli -p "$@"; }
for p in PORTS; do
  if rc $p ROLE | head -1 | grep -q slave; then rc $p CLUSTER RESET SOFT >/dev/null; fi
done
for p in PORTS; do
  rc $p FLUSHALL >/dev/null
  rc $p FUNCTION FLUSH >/dev/null
  rc $p CLUSTER RESET SOFT >/dev/null
  rc $p CONFIG SET cluster-node-timeout 1000 >/dev/null
done
"""

# The same table, read by every node of the shape: id, address, role, master
# and slots, without the `myself` mark and the timings.
VIEW = r"""redis-cli -p PORT CLUSTER NODES | awk '{
  sub("myself,", "", $3); line = $1 " " $2 " " $3 " " $4;
  for (i = 9; i <= NF; i++) line = line " " $i; print line }' | sort"""


def _script(text, **subst):
    for k, v in subst.items():
        text = text.replace(k, v)
    return text


class Cluster:
    """Both groups; each starts on first use."""

    def __init__(self, sh, image, cli, tag, valkey_image):
        self.groups = {"": Group(sh, image, cli, tag, PORTS),
                       "legacy-": Group(sh, LEGACY_IMAGE, cli, tag, LEGACY_PORTS),
                       "valkey-": Group(sh, valkey_image, cli, tag, VALKEY_PORTS, "valkey-server")}

    @staticmethod
    def group_of(shape):
        """The prefix naming a shape's group, and the group's ports."""
        for prefix, ports in (("legacy-", LEGACY_PORTS), ("valkey-", VALKEY_PORTS)):
            if shape.startswith(prefix):
                return prefix, ports
        return "", PORTS

    def reset(self, shape):
        prefix, _ = self.group_of(shape)
        self.groups[prefix].reset(shape[len(prefix):])

    def stop(self):
        for g in self.groups.values():
            g.stop()


class Group:
    """One image's nodes. `reset(shape)` before each run of a cluster case."""

    def __init__(self, sh, image, cli, tag, ports, server="redis-server"):
        self.sh, self.image, self.cli, self.ports, self.server = sh, image, cli, ports, server
        self.names = [f"cligate-node{p}-{tag}" for p in ports]
        self.started = False

    def _exec(self, script, check=True):
        r = self.sh(["docker", "exec", self.cli, "sh", "-c", script])
        if check and r.returncode != 0:
            raise RuntimeError(f"cluster fixture: {script[:80]!r}: "
                               f"{r.stdout.decode()}{r.stderr.decode()}")
        return r.stdout.decode()

    def _start(self):
        for port, name in zip(self.ports, self.names):
            # A config file, so CONFIG REWRITE (set-timeout) has one to write;
            # a replica's first sync starts at once rather than after 5 s.
            # And a node id fixed ahead of time (the port, eight times), so
            # outputs that name nodes are the same on every gate run.
            conf = (f"printf '{str(port) * 8} :{port}@{port + 10000} myself,master - 0 0 0 "
                    f"connected\\nvars currentEpoch 0 lastVoteEpoch 0\\n' > /data/nodes.conf && "
                    f"printf 'port {port}\\ncluster-enabled yes\\ncluster-config-file "
                    f"nodes.conf\\ncluster-node-timeout 1000\\nrepl-diskless-sync-delay 0"
                    f"\\nenable-debug-command yes"
                    f"\\nsave \"\"\\nappendonly no\\n' > /data/redis.conf && "
                    f"exec {self.server} /data/redis.conf")
            r = self.sh(["docker", "run", "-d", "--name", name, "--network", "host",
                         "--entrypoint", "sh", self.image, "-c", conf])
            if r.returncode != 0:
                raise RuntimeError(f"cluster fixture: could not start {name}: "
                                   f"{r.stderr.decode()}")
        self.started = True

    def stop(self):
        if self.started:
            self.sh(["docker", "rm", "-f", *self.names])

    def _own_nodes(self):
        """The nodes answering are the ones started here: a stray server on
        these ports would answer PING just as well, and every case would run
        against it."""
        ports = " ".join(map(str, self.ports))
        ids = self._exec(f"for p in {ports}; do redis-cli -p $p CLUSTER MYID; done").split()
        want = [str(p) * 8 for p in self.ports]
        if ids != want:
            raise RuntimeError(f"cluster fixture: ports {self.ports[0]}-{self.ports[-1]} are answered by "
                               f"other servers (ids {ids[:2]}...); stop them first")

    def _up(self):
        """A case may SHUTDOWN a node (del-node does); start it again."""
        ports = " ".join(map(str, self.ports))
        for _ in range(100):
            down = self._exec(f"for p in {ports}; do redis-cli -p $p PING >/dev/null 2>&1 "
                              f"|| echo $p; done", check=False).split()
            if not down:
                return
            for p in down:
                self.sh(["docker", "start", self.names[self.ports.index(int(p))]])
            time.sleep(0.1)
        raise RuntimeError("cluster fixture: nodes did not come back")

    def reset(self, shape):
        if shape not in SHAPES:
            raise RuntimeError(f"cluster fixture: unknown shape {shape!r}")
        if not self.started:
            self._start()
            self._up()
            self._own_nodes()
        self._up()
        self._exec(_script(RESET, PORTS=" ".join(map(str, self.ports))))
        at = self.ports
        masters = [(at[i], lo, hi) for i, lo, hi in SHAPES[shape][0]]
        replicas = [(at[i], at[m]) for i, m in SHAPES[shape][1]]
        for port, lo, hi in masters:
            if lo is not None:
                self._exec(f"redis-cli -p {port} CLUSTER ADDSLOTSRANGE {lo} {hi}")
        members = [m[0] for m in masters] + [r[0] for r in replicas]
        if not members:
            return
        for port in members[1:]:
            self._exec(f"redis-cli -p {members[0]} CLUSTER MEET 127.0.0.1 {port}")
        self._converge(members, lambda view: len(view) == len(members)
                       and all(" handshake" not in l and "fail" not in l for l in view))
        for port, master in replicas:
            self._exec(f"redis-cli -p {port} CLUSTER REPLICATE "
                       f"$(redis-cli -p {master} CLUSTER MYID)")
        self._converge(members, lambda view: sum(" slave " in l for l in view) == len(replicas))
        links = "; ".join(f"redis-cli -p {p} INFO replication | grep -c link_status:up"
                          for p, _ in replicas)
        states = "; ".join(f"redis-cli -p {p} CLUSTER INFO | grep -c state:ok" for p in members)
        want = len(replicas) + len(members)
        self._wait(lambda: self._exec(f"{links or 'true'}; {states}", check=False)
                   .split().count("1") == want)

    def _converge(self, members, done):
        """Every member holds the same table, and the table is `done`."""
        # One exec reads every member's table: a docker exec per node per
        # poll is most of a reset's time.
        script = "; echo ==; ".join(_script(VIEW, PORT=str(p)) for p in members)

        seen = []

        def agreed():
            views = [v.strip().splitlines() for v in self._exec(script).split("==\n")]
            seen[:] = views
            return all(v == views[0] for v in views) and done(views[0])
        # When it does not converge, say what each member last saw.
        self._wait(agreed, lambda: "\n".join(
            f"  {p} sees:\n" + "\n".join(f"    {l}" for l in v) for p, v in zip(members, seen)))

    def _wait(self, cond, explain=lambda: ""):
        deadline = time.time() + CONVERGE_S
        while not cond():
            if time.time() > deadline:
                raise RuntimeError("cluster fixture: the cluster did not converge\n" + explain())
            time.sleep(0.05)

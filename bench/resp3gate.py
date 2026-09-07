#!/usr/bin/env python3
"""Compare RESP3 reply SHAPES between the pinned redis and kevy.

Not the values — the type bytes. A reply that is `*` under RESP2 and `%` under
RESP3 has changed shape; one that is `$` in both has not. Redis decides which
verbs change, so redis is asked rather than a hand-written list, and kevy is
required to move exactly where redis moves.

Reads its own RESP so that no client library's renderer stands between the
wire and the comparison — the mistake compat3 made for its whole life, where
valkey-cli's raw mode printed `:1` and `+1` identically.
"""

import socket
import sys

# (label, argv, setup argv…) — setup runs on both servers first.
CASES = [
    ("HGETALL",      [b"HGETALL", b"h"],                          [[b"HSET", b"h", b"f1", b"v1", b"f2", b"v2"]]),
    ("CONFIG GET",   [b"CONFIG", b"GET", b"maxmemory"],           []),
    ("ZPOPMIN",      [b"ZPOPMIN", b"z"],                          [[b"ZADD", b"z", b"1", b"a", b"2", b"b"]]),
    ("ZSCORE",       [b"ZSCORE", b"z2", b"a"],                    [[b"ZADD", b"z2", b"1", b"a"]]),
    ("ZADD INCR",    [b"ZADD", b"z3", b"INCR", b"1", b"a"],       [[b"ZADD", b"z3", b"1", b"a"]]),
    ("ZINCRBY",      [b"ZINCRBY", b"z4", b"1", b"a"],             [[b"ZADD", b"z4", b"1", b"a"]]),
    ("HRANDFIELD WV",[b"HRANDFIELD", b"h2", b"2", b"WITHVALUES"], [[b"HSET", b"h2", b"f1", b"v1", b"f2", b"v2"]]),
    ("SPOP count",   [b"SPOP", b"s", b"1"],                       [[b"SADD", b"s", b"m1", b"m2", b"m3"]]),
    ("SMEMBERS",     [b"SMEMBERS", b"s2"],                        [[b"SADD", b"s2", b"m1"]]),
    ("GEOPOS",       [b"GEOPOS", b"g", b"P"],                     [[b"GEOADD", b"g", b"13.361389", b"38.115556", b"P"]]),
    ("XRANGE",       [b"XRANGE", b"st", b"-", b"+"],              [[b"XADD", b"st", b"*", b"k", b"v"]]),
]


def enc(argv):
    return b"*%d\r\n" % len(argv) + b"".join(b"$%d\r\n%s\r\n" % (len(a), a) for a in argv)


class Conn:
    def __init__(self, port, resp3):
        self.s = socket.create_connection(("127.0.0.1", port), 5)
        self.f = self.s.makefile("rb")
        if resp3:
            self.call([b"HELLO", b"3"])

    def call(self, argv):
        self.s.sendall(enc(argv))
        return self.read()

    def read(self):
        """Return the shape: type bytes, recursively, values discarded."""
        line = self.f.readline()
        if not line:
            return "EOF"
        t, rest = line[:1].decode(), line[1:].strip()
        if t in "+-:,#(":
            return t
        if t in "$=":
            n = int(rest)
            if n >= 0:
                self.f.read(n + 2)
            return t
        if t in "*~%>":
            n = int(rest)
            if n < 0:
                return t
            items = n * 2 if t == "%" else n
            return t + "[" + ",".join(self.read() for _ in range(items)) + "]"
        if t == "_":
            return t
        return f"?{t}"

    def close(self):
        try:
            self.s.close()
        except OSError:
            pass


def shapes(port):
    out = {}
    for label, argv, setup in CASES:
        for resp3 in (False, True):
            c = Conn(port, resp3)
            for s in setup:
                c.call(s)
            out[(label, resp3)] = c.call(argv)
            c.close()
    return out


def main() -> int:
    rport, kport = int(sys.argv[1]), int(sys.argv[2])
    r, k = shapes(rport), shapes(kport)

    bad, moved = [], 0
    print(f"{'verb':<16} {'redis r2->r3':<28} {'kevy r2->r3':<28} verdict")
    for label, _, _ in CASES:
        r2, r3 = r[(label, False)], r[(label, True)]
        k2, k3 = k[(label, False)], k[(label, True)]
        redis_moves = r2 != r3
        kevy_moves = k2 != k3
        if redis_moves:
            moved += 1
        if redis_moves and not kevy_moves:
            verdict = "MISSING — kevy sends the RESP2 shape under HELLO 3"
            bad.append(f"{label}: redis {r2} -> {r3}, kevy stays {k2}")
        elif redis_moves and k3 != r3:
            verdict = f"DIFFERS — kevy {k3}"
            bad.append(f"{label}: redis RESP3 {r3}, kevy RESP3 {k3}")
        elif not redis_moves and kevy_moves:
            verdict = "EXTRA — kevy changes where redis does not"
            bad.append(f"{label}: redis stays {r2}, kevy {k2} -> {k3}")
        else:
            verdict = "ok"
        print(f"{label:<16} {r2[:26]:<28} {k2[:26]:<28} {verdict}")

    if moved < 4:
        print(f"\nresp3gate: REFUSED — redis changed shape for only {moved} verbs; "
              "that is a broken probe, not a protocol with nothing in it", file=sys.stderr)
        return 2
    for line in bad:
        print(f"  {line}", file=sys.stderr)
    print(f"\nresp3gate: {'FAIL' if bad else 'ok'} — {moved} verbs change shape in redis, "
          f"{len(bad)} disagree")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())

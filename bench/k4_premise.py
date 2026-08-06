#!/usr/bin/env python3
"""K4 premise experiment: is cross-value redundancy real, and can a
per-datum baseline provably not reach it?

Research instrument only (zlib as oracle). Three encoders per corpus:
  per-datum   : each value compressed alone — the baseline K4 says cannot pass
  shared-dict : each value compressed with a shared zdict (dict counted once)
  segment     : the whole segment compressed as one stream — the ceiling
                for any cross-value capture

Four corpora, N=1000 values of ~400 B:
  identical : N copies of one value            (K4's literal shape)
  templated : JSON rows, same keys, varying values (the realistic shape)
  random    : os.urandom per value             (K2's adversarial shape)
  textual   : sentences over a shared vocabulary
"""
import json
import os
import random
import zlib

random.seed(7)
N, TARGET = 1000, 400


def pad(s: bytes) -> bytes:
    return (s + b" " * TARGET)[:TARGET]


def corpus_identical():
    v = pad(json.dumps({"id": 123456, "email": "ada@example.com",
                        "status": "active", "plan": "pro",
                        "note": "x" * 260}).encode())
    return [v] * N


def corpus_templated():
    out = []
    for i in range(N):
        row = {"id": 100000 + i,
               "email": f"user{i}@example{i % 7}.com",
               "status": random.choice(["active", "pending", "closed"]),
               "plan": random.choice(["free", "pro", "team"]),
               "created_at": 1700000000 + i * 37,
               "note": "".join(random.choice("abcdefgh ") for _ in range(230))}
        out.append(pad(json.dumps(row).encode()))
    return out


def corpus_random():
    return [os.urandom(TARGET) for _ in range(N)]


def corpus_textual():
    vocab = ("the order was shipped to the warehouse and the invoice "
             "was settled by the customer account after review").split()
    out = []
    for _ in range(N):
        out.append(pad(" ".join(random.choice(vocab) for _ in range(70)).encode()))
    return out


def per_datum(vals):
    return sum(len(zlib.compress(v, 6)) for v in vals)


def shared_dict(vals):
    # Dictionary: a 32 KiB sample of the corpus itself, counted in full.
    zdict = b"".join(vals[:: max(1, len(vals) // 80)])[:32768]
    total = len(zdict)
    for v in vals:
        c = zlib.compressobj(6, zlib.DEFLATED, -15, 9, 0, zdict)
        total += len(c.compress(v) + c.flush())
    return total


def segment(vals):
    return len(zlib.compress(b"".join(vals), 6))


for name, make in [("identical", corpus_identical), ("templated", corpus_templated),
                   ("random", corpus_random), ("textual", corpus_textual)]:
    vals = make()
    raw = sum(len(v) for v in vals)
    pd, sd, seg = per_datum(vals), shared_dict(vals), segment(vals)
    print(f"{name:<10} raw {raw:>7}  per-datum {pd:>7} ({raw/pd:5.2f}x)  "
          f"shared-dict {sd:>7} ({raw/sd:6.2f}x)  segment {seg:>7} ({raw/seg:7.2f}x)")
    print(f"{'':<10} per-value bytes: per-datum {pd/N:6.1f}  shared-dict {sd/N:6.1f}  "
          f"segment {seg/N:6.1f}")

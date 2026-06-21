# Axis B — big value sweep

**Hypothesis**: L1 `Value::ArcBulk` + `writev` iovec means kevy GET
on values > BULK_THRESHOLD (64 B) skips the per-GET memcpy of value
into per-conn output. Predicted kevy ≥120 % at -d 4 KB+ (where the
memcpy savings dominate).

## Methodology

- `redis-benchmark -c 50 -P 1 -t set,get -d {64,256,1K,4K,16K,64K}
  -n {600k/600k/400k/200k/100k/50k}` (n scaled down for big values
  to keep wall-clock reasonable; each scenario still runs 3 + 50k
  warm).
- kevy `--threads 2`; valkey/redis `--io-threads 10`; same lx64.
- 3 runs per cell, median reported.

## Result

| -d (B) | op  | kevy    | valkey  | redis   | **kevy / best** | verdict   |
|--------|-----|---------|---------|---------|-----------------|-----------|
|    64  | SET | 189 753 | 189 334 | 154 879 | 100 %           | tied      |
|    64  | GET | 195 822 | 189 753 | 159 363 | 103 %           | edge      |
|   256  | SET | 198 020 | 193 986 | 161 681 | 102 %           | edge      |
|   256  | GET | 196 014 | 191 327 | 157 233 | 102 %           | edge      |
|  1024  | SET | 192 123 | 185 271 | 156 617 | 104 %           | edge      |
|  1024  | GET | 193 892 | 188 413 | 151 573 | 103 %           | edge      |
|  4096  | SET | 170 503 | 172 861 | 143 266 |  99 %           | tied      |
|  4096  | GET | 175 902 | 172 712 | 139 567 | 102 %           | edge      |
| 16384  | SET | 138 313 | 137 741 | 122 850 | 100 %           | tied      |
| 16384  | GET | 139 860 | 139 082 | 118 906 | 101 %           | tied      |
| 65536  | SET |  66 138 |  69 541 |  57 737 |  95 %           | **❌ LOSS** |
| 65536  | GET |  69 541 |  71 633 |  61 881 |  97 %           | **❌ LOSS** |

## Interpretation

**Hypothesis NOT confirmed.** kevy is essentially TIED with valkey
across the full value-size sweep (99-104 %), and at -d 64 KB kevy
LOSES by 3-5 %.

Why the predicted ≥120 % didn't materialise:

1. **valkey's `tryAvoidBulkStrCopyToReply` (networking.c:1462)
   already does the same zero-copy that L1 brought to kevy.** Both
   servers writev the value bytes direct from storage without
   memcpy. The optimisation is at parity, not differential.
2. **valkey's `robj` refcount allows zero-overhead borrow for the
   iovec** (refcount++, write, refcount−−). kevy's `Arc<[u8]>::clone`
   is also one atomic, **same** order of magnitude. No structural
   delta.
3. **At -d 64 KB the workload is network-bandwidth-bound**, not
   per-op CPU-bound. `tcp_sendmsg` + IP/skb stack dominates; both
   servers hit the same loopback ceiling around 60-72 k ops/s. At
   that rate the kernel-stack overhead is the bottleneck for both;
   marginal differences in userspace evaporate.
4. **kevy's -d 64 KB LOSS** is tentatively attributed to:
   - Our writev iovec uses 3 iovecs (header / arc-body / CRLF) for
     a single bulk; valkey's batches multiple replies' iovecs into
     one writev when possible. With c50 concurrent, valkey can fuse
     replies across conns within a batch better.
   - kevy's per-shard arm_conns iterates 50/shard conns per iter,
     each potentially adding 3 iovecs — total iovec count grows
     non-linearly under high conn count.

## Honest verdict

**L1 brought kevy to PARITY with valkey on big values, not
super-position.** This is still a substantial win vs the pre-L1
state (-9 to -11 % at 10 KB GET), but it's not a kevy structural
differential — valkey was already doing the same trick. **Axis B
is NOT the path to ≥120 %.**

## Next-step ideas (for follow-up sprints, not this autorun)

- **Iovec coalescing at -c 50**: instead of one writev per conn per
  iter, gather iovecs across multiple ready conns into one writev
  batch (Linux 6.x allows iovec arrays of 1024). Could close the
  -d 64 KB gap.
- **MSG_ZEROCOPY (B5)**: kernel handles refcount on the user
  buffer until ACK; saves the page-cache copy on the kernel side.
  Worth re-attempting at -d ≥ 16 KB.

## Reproduce

```bash
ssh lx64
bash /root/kevy/bench/axis_b_bigval.sh
```

## Status

❌ **HYPOTHESIS NOT CONFIRMED.** kevy + valkey are essentially tied
(±5 %) across -d 64 B → 64 KB. L1 closed the pre-existing kevy
deficit but did not produce a ≥120 % structural win. Axis B does
NOT contribute to the ≥120 %-on-every-workload goal.

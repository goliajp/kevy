# Topic 05: io_uring multishot recv + provided buffers

**Status:** fixed (merged) — modest but consistent win; full potential measurement-blocked
**Severity:** medium
**First observed:** 2026-05-26 (follow-up to topic 04)

## Symptom

Topic 04's pipeline scan showed -c50 throughput rising 19× from -P1 to -P256,
implicating per-command io_uring SQE submit/re-arm overhead. The single-shot read
path submits one read SQE per ~16 commands at -P16 (vs ~256 at -P256) and re-arms
each on completion. Hypothesis: a **multishot recv** (one SQE re-fires per arrival,
kernel picking a buffer from a registered ring) cuts that to one recv per
connection, recovering up to ~2× at the typical -P16.

## Reproduction

```
bash bench/perf_pipe.sh /root/kevy_dev/target/release/kevy   # develop (single-shot)
bash bench/perf_pipe.sh /root/kevy/target/release/kevy        # feature (multishot)
```
(io_uring 4sh on cores 0-3, client 12 cores, GET -c50, vary -P.)

## Investigation log

- 2026-05-26 — Built the primitive in `kevy-sys` (io_uring_register / pbuf ring /
  `prep_recv_multishot` / `Completion::buffer_id`/`has_more` / `ProvidedBufRing`)
  with a unit test (one SQE → two completions in different buffers, recycled);
  passes on lx64 (kernel 6.12). Wired into `uring_reactor` (one shared 128×16K
  provided-buffer ring per shard; per-conn multishot recv; recycle each buffer
  after copying into `Conn::input`). sharded suite 11/11 via epoll AND io_uring.
- A/B (3 runs, `data/2026-05-26/multishot-ab.txt`): **-P16 +7.6%, -P64 +3.9%**,
  multishot ≥ develop in every run and **markedly more stable** (develop dips to
  3.01M/5.06M; multishot stays tight). NOT the ~2× hypothesized.

## Decision

**Merge.** Consistent positive (+4-7.6% at typical pipelines), lower variance,
less memory (one 2 MiB ring/shard vs 16 KiB/conn), the modern io_uring path, zero
regression, correct. The hypothesis that read-SQE overhead is THE -P16 bottleneck
was only **partly** right: on a single 16-core box -P16 is also CLIENT-bound (the
12-core redis-benchmark caps ~3.3M at -P16 regardless of server read-SQE count),
which masks most of the saving. So the merged win is modest; the true server-bound
gain awaits a dedicated load-gen box (topic 04's open measurement gap).

## Verification

sharded 11/11 (epoll + io_uring), clippy 0, A/B above. data:
`data/2026-05-26/multishot-ab.txt`. kevy-sys multishot unit test green on lx64.

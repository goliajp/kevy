# The pubsub residual has a name: page-refault zeroing — reclaim's other face

The v8 ledger recorded pub/sub at 0.858–0.894 with the allocator ON and
called it "a different residual class" without naming it. Profiled at
`c023ff8a` (per-word state), one-shard server under a looped
`kevy-pubsub-bench --subs 50 --size 64`, `perf record` 12 s per side:

| self-time | OFF (glibc) | ON (kevy-alloc) |
|---|---:|---:|
| `clear_page_erms` (kernel, zero-fill on first touch) | 12.2 % | **21.3 % — the top symbol** |
| `run_uring` | 23.4 % | 18.8 % |
| `deliver_publish` | 18.9 % | 14.2 % |

`clear_page_erms` is the kernel zero-filling a freshly faulted page.
Under glibc it exists too (12 %: the bench itself churns), but the ON
build nearly doubles it — because kevy-alloc **returns pages to the OS
and takes them back**. Every pub/sub burst allocates and frees delivery
buffers; reclaim hands the pages to the kernel; the next burst faults
them in again and pays the zeroing. glibc never gives pages back, so it
never pays re-entry. The +9 pp of kernel zeroing is the residual's
order of magnitude.

## This is the same mechanism R4 found, seen from the kernel's side

The collection-write decomposition
(`PERF-DECOMP-2026-08-06-collection-write-residual.md`) located the
write-side tax in the per-tick `thread_reclaim()` — and showed the tick
cannot simply be removed (the no-reclaim build wedges after ~300 M
accumulated ops; reclaim is load-bearing for liveness). This finding
adds the other half of the bill: what reclaim costs is not only its own
CPU but the **kernel's zero-fill on every page that comes back**.

One mechanism, two taxes, one design candidate: **page-return
hysteresis / pacing** — keep recently-hot pages parked under the
accounting's existing `hysteresis` term instead of returning them
eagerly, sized so M3's envelope still holds. That serves both residuals
at once, and it is measurable with the instruments already in place
(perfgate for the angles, allocgate-mem for M3, this profile for the
`clear_page_erms` share).

## Caveat

~15 % of ON samples sit in unresolved `0x162xxx` frames (a stripped
system DSO — likely libc's syscall/memcpy internals); the named story
does not depend on them, but a follow-up with a debug libc would close
the gap.

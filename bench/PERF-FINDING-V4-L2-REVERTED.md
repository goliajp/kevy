# T9 L2 finding — the idle ladder is productive waiting, not tax

Verdict: L2 (under-saturation closure via blocking-enter redesign)
is REFUTED and the attack was fully reverted. The evidence, from a
complete implementation measured A/B on the bench box:

- Earlier blocking (spin 32 + deadline enter) put park/wake latency
  onto the request path: c50 GET -16.6%, SET -28.5%, -c1 -39%.
- The same redesign at spin 256 measured flat everywhere (±0 across
  c50/c100/legacy-P256) — the ~1,050 cycles/op the ladder burns at
  c50 is the SHAPE of waiting, not recoverable cost. Removing it
  converts spin into sleep; throughput does not move.
- The decomposition's dual-client "+25%" evidence re-reads as a
  redis-benchmark client-side in-flight limit, not server
  under-saturation; the c100 cycles/op drop is an effect of higher
  iteration density, not a cause a server idle policy can create.

Positive residue, archived for future use: a min_wait-based
aggregated park (IORING_ENTER_EXT_ARG + IORING_FEAT_MIN_TIMEOUT,
kernel-verified on 6.12) fully carried the deaf-nap's legacy-angle
duties — if the nap state machine is ever to be removed, that path
is validated. Full diff parked in the session archive.

Next lever by the data: L3 (per-CQE -> per-conn batching) or accept
the c50 plateau as a joint client-side limit.

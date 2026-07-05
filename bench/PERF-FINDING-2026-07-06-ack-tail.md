# v3.14 OPEN — ACK tail lag: last N frames' ack never converges

## Fixed along the way (real, committed)

1. Poll reactor ran the replication pump only inside `if did_work` —
   idle iterations sent no heartbeats and drained no ACKs. Moved out
   (uring side was already correct).
2. Primary's ACK reader wedged forever on any non-ACK byte at the
   input head (first ack=1 landed, everything after queued behind
   residue). Now a tolerant reader: unknown lines skip to CRLF.
3. min-replicas chicken-and-egg: a fresh empty replica can never ack
   >0 before the first write, and the first write needs a healthy
   replica. View's acked is now Option — Some(0) from a heartbeat
   round trip counts as healthy.

## Still open (availgate clamp 5 red, branch unmerged)

After a write burst, slave0 acked sticks N frames (2-10, varies per
run) below sent and never converges, EVEN THOUGH:
- runner-side probes show from_offset == primary_offset on every
  ping (replicas believe they are caught up),
- link stays up (pings keep arriving → the output FIFO drains),
- data plane converges (replica dbsize/GET complete).

Contradiction to resolve: if frames sat in the primary output FIFO
the pings behind them couldn't arrive either. Hypotheses for the
next session (decompose, don't guess):
- sent_offset advances at APPEND time, not at socket-write time —
  "sent" may simply be ahead of the wire and the ack correctly
  reflects wire truth... but then pings behind those bytes couldn't
  flow (they do). Measure bytes-on-wire vs sent_offset directly.
- per-shard identity mismatch: is the slave0 view row and the ACK
  slot really the same conn? Probe replica_id at drain vs at tick.
- drain reads only fire when the pump iter runs on THAT shard;
  verify the wedged shard's iter cadence.

Repro: bash bench/availgate.sh target/release/kevy (clamp 5).

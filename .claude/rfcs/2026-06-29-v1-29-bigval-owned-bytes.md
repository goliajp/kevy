# RFC v1.29 — bigval owned-bytes plumbing (B3)

**Status**: design lock; implementation deferred to v1.29.0 sprint
**Author**: GOLIA K.K.
**Date**: 2026-06-29
**Anchors**:
- [bench/PERF-DECOMP-2026-06-28-bigval-SET-vs-valkey-9.1.md](../../bench/PERF-DECOMP-2026-06-28-bigval-SET-vs-valkey-9.1.md) — Phase A decomp + §I perf record verification
- [bench/PERF-PROBE-2026-06-28-axis-sweep-vs-valkey.md](../../bench/PERF-PROBE-2026-06-28-axis-sweep-vs-valkey.md) — probe identifying the losing workload
- methodology: `/Users/doracawl/.claude-shared/global/methodology/perf-decomposition-vs-polish.md`

## Why

At `-d 65536` SET, kevy loses to valkey 9.1 by 8% (probe). Phase A decomp + perf record confirmed kevy spends ~15.92% userspace CPU on libc memcpy vs valkey's 4.94% — **~3.2× more**. Source-level traced to **two extra userspace memcpys per SET**:

- **MEMCPY #1**: slab → frame Vec at `crates/kevy-rt/src/uring_bigbulk.rs:87/115` (`extend_from_slice`)
- **MEMCPY #2**: frame → Arc at `crates/kevy-store/src/string.rs:38` (`Arc::from(&[u8])` inside `pick_value_for_set`)

valkey 9.1 avoids both via `read()`-into-sized-sds + `createObject(OBJ_STRING, c->querybuf)` adoption (networking.c:3799+4249) — zero extra copies of the value body.

The fix per Phase A Top-N: **B3 — plumb owned-Vec value-bulks through ArgvBorrowed → cmd_set → store.set + cross-shard `Inbound::RequestBatch`**. This RFC pins down the exact code-anchored plan.

## L1 lock — version line

- This work ships as **v1.29.0** (workspace minor bump). Pure perf/architecture, no API break.
- v1.27 was Lua-only (closed). v1.28 was workflow infra (closed). v1.29 = bigval write-path. **Subsequent non-bigval fixes during the v1.29 sprint go to v1.29.x patch line, not v1.30**.

## Non-goals

- Not changing the read-side (GET) at this stage. The -d 65536 GET gap is only 3%; ArcBulk + writev already gets us most of the way.
- Not changing small-payload paths. The 0-64KB range is already winning vs valkey at the measured workloads.
- Not addressing axis H pub/sub 4KB (which also loses -3%). Different code path; separate RFC after v1.29 ships.

## What changes

### File 1: `crates/kevy-rt/src/uring_conn.rs`

Replace the single-Vec frame with split header/body buffers:

```rust
pub(crate) struct BigArgState {
    /// Verb + key prefix + the big-value length-header (`*<argc>\r\n
    /// $<verblen>\r\nSET\r\n$<keylen>\r\n<key>\r\n$<bulklen>\r\n`).
    /// Bounded small — proto headers + verb + key bytes only.
    pub(crate) header: Vec<u8>,
    /// Big value body bytes. Capacity = `body_len + 2` so the trailing
    /// CRLF lands in the same Vec without realloc. Bytes 0..body_len are
    /// the value; bytes body_len..body_len+2 are CRLF.
    pub(crate) body: Vec<u8>,
    /// Target body length (the N in `$<N>\r\n`). Body is complete when
    /// `body.len() == body_len + 2` (including trailing CRLF).
    pub(crate) body_len: usize,
    /// Parsed (verb, key) — populated at promote time so the dispatch
    /// path doesn't have to re-parse the header. Lifts `cmd_set` /
    /// `cmd_setex` / etc. directly without going through `dispatch_batch`.
    pub(crate) promoted_cmd: PromotedCmd,
}

/// Variants the BigBulk promote path supports — bare last-bulk-big SETs.
/// Mirrors the `Supported verb` set in `uring_bigbulk_probe.rs`. Each
/// carries the parsed (key, maybe ttl) so dispatch only needs the body.
pub(crate) enum PromotedCmd {
    Set { key: Vec<u8> },
    Setex { key: Vec<u8>, ttl_secs: u64 },
    Psetex { key: Vec<u8>, ttl_ms: u64 },
    Append { key: Vec<u8> },
    GetSet { key: Vec<u8> },
    MsetLast { prior: Argv, key: Vec<u8> }, // prior holds the n-1 already-staged k/v pairs
}
```

### File 2: `crates/kevy-rt/src/uring_bigbulk_probe.rs`

Probe needs to yield enough info to populate `PromotedCmd`. Today it returns `BigArgGenericProbe::Promote { total, bytes_present }`. Extend:

```rust
pub(crate) enum BigArgGenericProbe {
    None,
    NeedMore,
    Promote {
        /// Total RESP frame length (header + body + CRLFs). For the
        /// split-buf layout this is `header.len() + body_len + 2`.
        total: usize,
        bytes_present: usize,
        /// Parsed verb + key (and ttl/prior args where applicable).
        cmd: PromotedCmd,
        /// Length of the header prefix (everything up to and including
        /// the `$<N>\r\n` line of the big bulk). `tail[..header_len]`
        /// is the header bytes; `tail[header_len..]` is the start of
        /// body.
        header_len: usize,
        /// Body length (the N). Convenience field — equals
        /// `total - header_len - 2`.
        body_len: usize,
    },
}
```

The probe was already doing this parse internally (it has to read the verb and find the last bulk's header). Just expose it.

### File 3: `crates/kevy-rt/src/uring_bigbulk.rs`

Rewrite `try_promote_bigbulk` to split the initial tail:

```rust
pub(crate) fn try_promote_bigbulk(...) -> bool {
    let probe = probe_generic_bigbulk(tail);
    let BigArgGenericProbe::Promote { total, bytes_present, cmd, header_len, body_len } = probe
        else { return false; };
    let Some(uc) = io.get_mut(&cid) else { return false; };
    if uc.pending_big_arg.is_some() { return false; }
    if total > MAX_BULK_LEN + 1024 { return false; }

    // header bytes always present in the tail head (probe parsed them)
    let header = tail[..header_len].to_vec();
    // body bytes present in tail (may be 0)
    let body_present = bytes_present.saturating_sub(header_len).min(body_len + 2);
    let mut body = Vec::with_capacity(body_len + 2);
    body.extend_from_slice(&tail[header_len..header_len + body_present]);

    if body.len() == body_len + 2 {
        // entire frame in one slab — dispatch immediately
        self.dispatch_promoted_cmd(cid, cmd, body, body_len, io);
        return true;
    }
    uc.pending_big_arg = Some(Box::new(BigArgState { header, body, body_len, promoted_cmd: cmd }));
    true
}
```

And `uring_bigbulk_feed` becomes a focused body-Vec append:

```rust
pub(crate) fn uring_bigbulk_feed(...) {
    let Some(uc) = io.get_mut(&cid) else { return; };
    let Some(state) = uc.pending_big_arg.as_mut() else { return; };
    let need = state.body_len + 2 - state.body.len();
    let take = slab.len().min(need);
    if take > 0 {
        state.body.extend_from_slice(&slab[..take]);  // memcpy #1 — still here
    }
    if state.body.len() == state.body_len + 2 {
        let state = uc.pending_big_arg.take().expect("just observed");
        // Strip trailing CRLF before passing to Store
        let mut body = state.body;
        body.truncate(state.body_len);
        self.dispatch_promoted_cmd(cid, state.promoted_cmd, body, state.body_len, io);
    }
    if take < slab.len() {
        self.uring_bigbulk_feed_pipelined(cid, io, &slab[take..]);
    }
}
```

### File 4: new dispatch — `crates/kevy-rt/src/uring_bigbulk.rs`

```rust
fn dispatch_promoted_cmd(
    &mut self,
    cid: u64,
    cmd: PromotedCmd,
    body: Vec<u8>,         // owned body bytes, exactly body_len long
    body_len: usize,
    io: &mut KevyMap<u64, UringConn>,
) {
    // Cross-shard guard — see "Routing" section below
    let shard_for_key = self.shard_of_key(cmd.key());
    if shard_for_key == self.id {
        // Local fast path — owned Vec adopted by Store::set without
        // memcpy #2.
        match cmd {
            PromotedCmd::Set { key } => {
                let _ok = self.store.set(&key, body, None, false, false);
                self.write_simple_ok(cid);
            }
            PromotedCmd::Setex { key, ttl_secs } => {
                let _ok = self.store.set(&key, body, Some(Duration::from_secs(ttl_secs)), false, false);
                self.write_simple_ok(cid);
            }
            // ... other variants
        }
        // AOF / replication hook with owned body
        self.aof_log_promoted(&cmd, &body);
        return;
    }
    // Cross-shard — package as Inbound::RequestBatch carrying owned body.
    // Body Vec moves into the Inbound message; no memcpy.
    self.send_promoted_inbound(shard_for_key, cid, cmd, body);
}
```

### File 5: `crates/kevy-rt/src/inbox.rs`

Extend the cross-shard `Inbound` enum to carry an owned big-value variant:

```rust
pub enum Inbound {
    // ... existing variants ...
    /// Cross-shard promoted-bigarg command. Body Vec moves with the
    /// message — no memcpy at the shard boundary.
    PromotedRequest {
        from_cid: u64,
        cmd: PromotedCmd,
        body: Vec<u8>,
    },
}
```

Owner-shard handler unpacks and calls the same local fast path as above.

### File 6: `crates/kevy/src/dispatch*.rs` — no change

This is the wire that the v1.25 B.4 retire was about. The new path is *separate* from `dispatch_batch` — it never enters the normal command dispatch pipeline. So the existing dispatch is untouched. **Risk of dispatch regression: zero.**

### File 7: `crates/kevy-resp/src/argv.rs` — no change

Argv stays as-is. The promoted path uses `PromotedCmd` instead of constructing an Argv.

## Correctness — cross-shard routing

The v1.25 B.4 retire reason was: `self.store.set` writes to the owning shard's Store, not the key's hash-target shard. The fix in B3:

1. `dispatch_promoted_cmd` computes `shard_for_key = self.shard_of_key(cmd.key())`.
2. If `shard_for_key == self.id`: local fast path (saves both memcpys).
3. Else: `Inbound::PromotedRequest { cmd, body }` sent to `shard_for_key`. Body Vec moves through the channel — no memcpy. Receiver-side handler runs the same local fast path.

The cross-shard message still costs the channel send, but the value bytes never get copied. valkey's `tryAvoidBulkStrCopyToReply` has the same architectural shape.

## Correctness — slab vs body_buf in flight

Multishot recv stays armed throughout — no cancel/rearm. The slab→body memcpy in `uring_bigbulk_feed` remains. **B2-alt (eliminate memcpy #1) is OUT of v1.29.0 scope** — it's an additional 150 LOC of io_uring semantic risk for ~3-7 µs/op extra gain on top of B3's 3-7 µs/op. v1.29.x patch line if needed.

## Correctness — AOF and replication

Today both AOF and replication consume the parsed RESP frame from `dispatch_batch`. Promoted path bypasses `dispatch_batch`, so it must:
- AOF: re-serialize the owned cmd+body into RESP (`*3\r\n$3\r\nSET\r\n$<keylen>\r\n<key>\r\n$<body_len>\r\n<body>\r\n`) into the AOF group buffer. Same bytes as the original wire — one writev not a memcpy if we keep the body Vec borrow. Implementation detail; deferred.
- Replication: same. The replica wire is the same RESP.

## Test plan

- Unit: `BigArgState` field shapes; `PromotedCmd` variants; probe extraction of `header_len`/`body_len`/`cmd`.
- Integration: 6 SET-shape commands × 3 conn-density cases × {local-shard, cross-shard} = 36 cases.
- Cross-shard reverse-check: SET via promoted path, GET via normal path, value byte-identical.
- Multishot CQE interaction: single CQE-delivers-everything case; multi-CQE-delivers-body case; multi-CQE-with-pipelined-suffix case.

## Perf validation gate

- `bench/axis_b_bigval.sh` baseline (current develop): kevy -d 65536 SET = 63.6k.
- B3 target: ≥ 72k (matches valkey within noise, ideally above).
- `bench/perfgate.sh` must remain green for all other workloads.

## Implementation order

Ship as one tight commit chain on `feature/v1-29-bigval-owned-bytes`:

1. **C1** — extend `BigArgGenericProbe::Promote` with `cmd`, `header_len`, `body_len`. Probe unit tests updated. No behavior change yet (consumers still use the old fields).
2. **C2** — refactor `BigArgState` to the split header/body layout. Probe consumers updated to populate the new fields. Existing `dispatch_batch` path still used at completion (reassembles header+body into a frame Vec). **No perf change**; this is the refactor commit. Tests stay green.
3. **C3** — add `PromotedCmd` enum + `dispatch_promoted_cmd` local fast path (shard == self.id only). At completion, route promoted cmds through the new path instead of `dispatch_batch`. **Eliminates memcpy #2 on local-shard SETs**. `dispatch_batch` still used on cross-shard branches.
4. **C4** — cross-shard `Inbound::PromotedRequest` plumbing. Receiver-side handler. **Eliminates memcpy #2 on cross-shard SETs.**
5. **C5** — AOF + replication serialization on the promoted path.
6. **C6** — perfgate on lx64 (kevy 2 cores vs kevy 2 cores baseline + valkey 10 cores). Compare to PERF-DECOMP-2026-06-28-bigval-SET-vs-valkey-9.1.md §I baseline (kevy 15.92% userspace memcpy → expect 5-7%).
7. **C7** — `chore(release): v1.29.0` workspace bump + tag.

Each Cn is a self-contained commit that compiles, passes tests, and can be reverted independently.

## Risks

- **R1 — AOF format**: if AOF format hides any subtle assumption about command shape, the re-serialized promoted path may produce a wire-different log. Mitigation: AOF roundtrip test that does promoted-SET then loads back and reads the value.
- **R2 — cross-shard message size cap**: `Inbound::PromotedRequest` carrying a 64KB Vec may exceed the channel's typical small-message size budget. Mitigation: check `kevy_ring`'s message-size limits; if needed, box the body. Body is already heap-allocated so Box wrap is cheap.
- **R3 — pipelined commands after a promoted SET**: `uring_bigbulk_feed_pipelined` already exists; verify pipelined GET-after-promoted-SET reads the updated value (it should — local fast path commits to the same Store).
- **R4 — multishot recv CQE arriving for promoted conn while body is being filled**: this is the existing happy path; no change.

## What this does NOT close

- The 3% GET gap at -d 65536 — handled by ArcBulk+writev today; future polish if needed.
- The 3% pubsub gap at 4KB msgs — different path (publish broadcast), separate RFC.

## Decision summary

v1.29.0 = B3 SET-side write-path bigval owned-bytes plumbing. 7-commit chain, ~200-300 LOC across 5 files. Closes the -d 65536 SET 8% gap; expected to invert lead to ~2-5% above valkey. Cross-shard correctness via Inbound::PromotedRequest, not via post-hoc fix.

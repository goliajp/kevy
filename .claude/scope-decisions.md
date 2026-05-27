# kevy scope decisions

Project-level "what's IN vs OUT of scope, and why" log. Append-only —
older entries stay as historical context, newer entries refine the
boundary. CLAUDE.md links here for non-obvious calls.

---

## OUT of v1.0 — and beyond

### Bare-metal MCU (`no_std`) port

**Decided:** 2026-05-27, by user

**What this excludes:** Cortex-M3/M4/M7 (STM32, nRF52, RP2040), ESP32-S2,
RISC-V MCUs (ESP32-C3, GD32V), and similar "no operating system,
running directly on hardware" embedded targets with typically
16-512 KB SRAM.

**Why:** porting kevy would be a full rewrite, not an adaptation:
- `std` not available → must move to `#![no_std]` + `alloc` (or
  `heapless` for bounded compile-time collections)
- No default heap allocator → need explicit `linked_list_allocator`
  config or all-static layout
- No threads / no OS scheduler → kevy-rt's thread-per-core reactor
  is meaningless; embedded mode would also need single-task rewrite
- `format!` machinery is a code-size grenade on MCUs (tens of KB)
- Stack typically 1-4 KB; would need full audit of recursion + locals
- Cortex-M0 has no atomics at all; M3+ has only a subset
- 32-bit pointer width (same blocker as wasm32, but only one of many)

This is "port to a different product" scale, not "add a feature".
SQLite's MCU support took years of engineering on a dedicated branch.

**What we DO support that the user might call "IoT":** Linux SBCs —
Raspberry Pi 4/5, Jetson Nano, Rock Pi, OrangePi, OpenWrt high-end,
Buildroot, Yocto. These ship full `std` + glibc/musl and run kevy
**with zero changes** on `aarch64-unknown-linux-gnu` or
`aarch64-unknown-linux-musl`.

**Re-evaluate:** only if a paying customer asks specifically for bare-
metal MCU. The right answer at that point would likely be a separate
`kevy-tiny` fork, not in-tree refactoring.

### AUTH / TLS

**Decided:** 2026-05-27, by user (v1.0 scope discussion)

Deferred to v0.3+ / v2 timeline. Target deployment scenarios
(docker-compose internal network, kubernetes pod network, embedded
in-process, browser/WASM, cache layer fronted by trusted upstream)
all have the trust boundary at the *network* level, not the database
level. Matches valkey/redis default behavior (no `requirepass`).

Mitigations in v1.0:
- `bind` defaults to `127.0.0.1` (loopback only)
- Startup WARN if non-loopback bind is set (`kevy WARN: bind=… is
  not loopback and kevy has no AUTH/TLS yet.`)

Re-evaluate when: public internet exposure becomes a real target, or
multi-tenant kubernetes deployment with untrusted neighbor pods is
in scope.

### Replication (single-machine primary→replica)

**Decided:** 2026-05-27, by user

Cut from v0.2 / v1.0 because target scenarios (dev / docker-compose
internal / embedded / cache) don't need it:

- docker-compose: single instance is the norm; HA = upgrade path to
  k8s StatefulSet + persistent volume
- embedded: in-process library, no "replicate" concept
- cache: upstream DB is the source of truth, cache rebuild acceptable

Root data-loss protection is `crash-safe persistence` (the AOF
rewrite + verify track in v1.0 scope), not replication.

Re-evaluate when: a real "99.9% uptime, can't tolerate 5s restart"
prod scenario arrives. Even then, k8s StatefulSet path may suffice
without in-process replication.

### Cluster mode

**Decided:** prior session, by user (CLAUDE.md kept as-is)

**Permanent OUT.** kevy is single-machine only. Thread-per-core
sharding stays (it's an on-machine perf technique, not distribution).

Cluster mode would invalidate `kevy-hash`'s no-DoS / no-random-seed
trade-off (the keyspace would cross trust boundaries).

If a customer needs cluster, the right tool is twemproxy / envoy
cluster routing in front of N independent kevy instances, not
in-kevy cluster code.

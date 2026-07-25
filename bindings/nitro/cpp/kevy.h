/* kevy.h — C ABI for embedding kevy.
 *
 * One generic command entry: kevy_cmd() takes argv and returns the
 * RESP-encoded reply, the same bytes a kevy server would send. All verbs
 * are reachable through it; pair it with any small RESP parser.
 *
 * Pub/sub is polled: kevy_sub_next() drains one frame at a time, encoded
 * as the RESP array the server would push.
 *
 * Every function is safe to call from multiple threads on the same handle.
 * Handles must be closed exactly once; buffers freed exactly once.
 *
 * ABI version: KEVY_ABI (bumped only on breaking signature changes).
 */
#ifndef KEVY_H
#define KEVY_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define KEVY_ABI 1

typedef struct KevyDb KevyDb;   /* opaque database handle */
typedef struct KevySub KevySub; /* opaque subscription handle */

/* A buffer owned by kevy. Never free() it — free it with the function paired
 * to the call that produced it (kevy_buf_free for most, kevy_buf_free_shared
 * for kevy_get_shared; passing a shared-lane buffer to kevy_buf_free corrupts
 * the heap). All three fields must reach that free untouched. */
typedef struct {
  uint8_t *ptr;
  size_t len;
  size_t cap;
} KevyBuf;

/* ── lifecycle ─────────────────────────────────────────────────────── */

/* Open a persistent store rooted at dir (UTF-8, dir_len bytes, no NUL
 * needed). Replays its log on open. NULL on failure. */
KevyDb *kevy_open(const uint8_t *dir, size_t dir_len);

/* Open a pure in-memory store — nothing survives the process. */
KevyDb *kevy_open_mem(void);

/* Close the store. The handle must not be used afterwards. */
void kevy_close(KevyDb *db);

/* ── commands ──────────────────────────────────────────────────────── */

/* Execute one command; argv[0] is the verb ("SET", "IDX.CREATE", …).
 * Writes the RESP reply to *out. Returns 0 on success — note a protocol
 * error (-ERR …) is still a successful call with a RESP error in *out.
 * Non-zero = the call itself was misused; *out is empty, do not free. */
int32_t kevy_cmd(KevyDb *db, size_t argc, const uint8_t *const *argv,
                 const size_t *argv_len, KevyBuf *out);

/* Scalar fast path — GET/SET without argv or RESP framing. The lane the
 * mobile bindings' hot loop lives on. kevy_get: 1 hit / 0 miss / neg
 * misuse; kevy_set: 0 ok / neg misuse (ttl_ms 0 = no TTL). */
int32_t kevy_get(KevyDb *db, const uint8_t *key, size_t key_len, KevyBuf *out);
int32_t kevy_set(KevyDb *db, const uint8_t *key, size_t key_len,
                 const uint8_t *val, size_t val_len, uint64_t ttl_ms);

/* Batch SET — apply n sets in one crossing (amortizes the per-call boundary a
 * binding pays once per key on a bulk load). keys[i]/vals[i] point at
 * key_lens[i]/val_lens[i] bytes. Durability is unchanged from kevy_set. 0 ok /
 * -1 misuse / -2 store error. */
int32_t kevy_set_many(KevyDb *db, size_t n, const uint8_t *const *keys,
                      const size_t *key_lens, const uint8_t *const *vals,
                      const size_t *val_lens);

/* Zero-copy GET: a bulk value comes back as an Arc clone (refcount bump, no
 * byte copy) whose bytes the returned buffer VIEWS — the analog of MMKV
 * returning a view of its mmap page. In the out KevyBuf, ptr/len are the value
 * view; cap is an OPAQUE owner handle. Free ONLY with kevy_buf_free_shared()
 * (NOT kevy_buf_free). 1 hit / 0 miss / neg misuse. */
int32_t kevy_get_shared(KevyDb *db, const uint8_t *key, size_t key_len, KevyBuf *out);

/* Free a KevyBuf from kevy_get_shared(): pass all three fields unchanged (cap
 * is a tagged owner handle — an Arc for a bulk value or a Vec for a small
 * one). Pairs 1:1 with kevy_get_shared; do NOT mix with kevy_buf_free. */
void kevy_buf_free_shared(uint8_t *ptr, size_t len, size_t cap);

/* Free any KevyBuf returned by this library: pass its three fields
 * unchanged. Scalars, not the struct by value — indirect struct passing
 * (AArch64, >16 bytes) is beyond several FFI loaders. Null ptr = no-op. */
void kevy_buf_free(uint8_t *ptr, size_t len, size_t cap);

/* ── pub/sub (polled) ──────────────────────────────────────────────── */

/* Subscribe to one channel / one glob pattern. NULL on failure. */
KevySub *kevy_subscribe(KevyDb *db, const uint8_t *chan, size_t chan_len);
KevySub *kevy_psubscribe(KevyDb *db, const uint8_t *pat, size_t pat_len);

/* Scalar publish — PUBLISH without the RESP round-trip (no argv packing, no
 * reply buffer). Delivers to every in-process subscriber (direct + pattern)
 * and returns the receiver count; -1 on misuse. */
int64_t kevy_publish(KevyDb *db, const uint8_t *chan, size_t chan_len,
                     const uint8_t *payload, size_t payload_len);

/* ── open report ───────────────────────────────────────────────────── */

/* The boot-replay verdict: what the open restored, and what it could not.
 * dropped_bytes > 0 or corrupt != 0 means the store recovered LESS than
 * its files held (the dropped region was quarantined next to the AOF as
 * aof-<id>.aof.corrupt-quarantine.<ts>) — surface it as a startup health
 * check. */
typedef struct KevyOpenReport {
    uint64_t replayed_commands;
    uint64_t replayed_bytes;
    uint64_t elapsed_ms;
    uint64_t dropped_bytes;
    uint8_t corrupt;
    uint32_t quarantine_count;
} KevyOpenReport;

/* Fill *out with db's open verdict. 0 = ok, -1 = misuse. */
int32_t kevy_open_report(KevyDb *db, KevyOpenReport *out);

/* ── lifecycle ─────────────────────────────────────────────────────── */

/* Options for kevy_open_with. Start from KEVY_OPEN_OPTIONS_INIT (the
 * exact defaults kevy_open uses) and override what you need. rewrite_pct
 * 0 turns the growth rule off (as in Redis); rewrite_bytes /
 * rewrite_interval_secs are the absolute-size and staleness triggers
 * (0 = off). fsync: 0 everysec, 1 always, 2 no. */
typedef struct KevyOpenOptions {
    uint8_t fsync;
    uint32_t shards;
    uint32_t rewrite_pct;
    uint64_t rewrite_min_size;
    uint64_t rewrite_bytes;
    uint64_t rewrite_interval_secs;
} KevyOpenOptions;

#define KEVY_OPEN_OPTIONS_INIT \
    { 0, 0, 100, (uint64_t)64 * 1024 * 1024, 0, 0 }

/* kevy_open with explicit options: durable at dir when dir != NULL,
 * in-memory when dir == NULL && dir_len == 0. NULL opts = kevy_open's
 * defaults. NULL on failure. */
KevyDb *kevy_open_with(const uint8_t *dir, size_t dir_len,
                       const KevyOpenOptions *opts);

/* Flush every shard's AOF (a REAL fsync), write the feed continuity
 * marker, then refuse every later write (reads stay available) — the
 * deterministic teardown: kevy_shutdown(db); exit(0). Idempotent.
 * 0 = ok, -1 = misuse, -2 = I/O failure (store still usable; retry). */
int32_t kevy_shutdown(KevyDb *db);

/* Drain one pending frame without blocking.
 * 1 = frame written to *out (RESP array: message / pmessage / acks);
 * 0 = nothing queued; negative = misuse. */
int32_t kevy_sub_next(KevySub *sub, KevyBuf *out);

/* Block until one frame is queued or timeout_ms elapses (0 = wait forever).
 * Parks the thread — no busy-poll — so a push-style poller can wait in the
 * kernel instead of spinning kevy_sub_next.
 * 1 = frame written to *out; 0 = timeout; negative = misuse / bus closed. */
int32_t kevy_sub_wait(KevySub *sub, uint64_t timeout_ms, KevyBuf *out);

/* Scalar drain — the raw message *payload* only, no RESP framing (the
 * pub/sub analog of kevy_get). Skips control/ack frames, moves the payload
 * into *out. For a known-channel push subscriber that only wants the bytes;
 * the channel and message-vs-pmessage distinction are lost (a pattern
 * subscriber that needs the channel keeps using kevy_sub_next).
 * 1 = payload written to *out; 0 = nothing queued; negative = misuse. */
int32_t kevy_sub_next_raw(KevySub *sub, KevyBuf *out);

/* Blocking kevy_sub_next_raw: parks up to timeout_ms (0 = wait forever).
 * 1 = payload written to *out; 0 = timeout OR a control/ack frame was
 * consumed (no payload) — re-wait; negative = misuse / bus closed. */
int32_t kevy_sub_wait_raw(KevySub *sub, uint64_t timeout_ms, KevyBuf *out);

/* Close the subscription (unsubscribes everything it held). */
void kevy_sub_close(KevySub *sub);

/* ── meta ──────────────────────────────────────────────────────────── */

/* Engine version, e.g. "4.0.0". Static string — do not free. */
const char *kevy_version(void);

/* Runtime ABI version; compare against KEVY_ABI at startup. */
uint32_t kevy_abi(void);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* KEVY_H */

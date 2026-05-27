// Cross-language counterpart of kevy-hash's bench, C side:
// xxHash (XXH3_64bits) + wyhash on the same hash_bytes / hash_u64
// workloads. Emits one JSON line per measurement.
//
// xxHash and wyhash are the two dominant non-crypto hashes in the C
// ecosystem (xxHash is the long-standing speed leader; wyhash is the
// newer favourite for hash-table workloads and is what Go switched
// to internally in some places).
//
// Both vendored as single-header — XXH_INLINE_ALL emits XXH3 inline.

#define _POSIX_C_SOURCE 199309L
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define XXH_INLINE_ALL
#include "xxhash.h"
#include "wyhash.h"

#define ITER     1000000
#define SAMPLES  25
#define HOST     "M4-Pro-aarch64"
#define STONE    "kevy-hash"
#define LANG     "c"

static volatile uint64_t g_sink = 0;
static void __attribute__((noinline)) sink_u64(uint64_t x) { g_sink ^= x; }

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static int cmp_u64(const void* a, const void* b) {
    uint64_t va = *(const uint64_t*)a, vb = *(const uint64_t*)b;
    return (va > vb) - (va < vb);
}

static void emit_json(const char* competitor, const char* workload,
                      uint64_t med, uint64_t p95, uint64_t min) {
    char buf[32];
    time_t t = time(NULL);
    strftime(buf, sizeof(buf), "%Y-%m-%dT%H:%M:%SZ", gmtime(&t));
    printf(
        "{\"stone\":\"%s\",\"language\":\"%s\",\"competitor\":\"%s\","
        "\"workload\":\"%s\",\"metric\":\"ns_per_op\","
        "\"value_median\":%llu,\"value_p95\":%llu,\"value_min\":%llu,"
        "\"iterations\":%d,\"host\":\"%s\",\"date\":\"%s\"}\n",
        STONE, LANG, competitor, workload,
        (unsigned long long)med, (unsigned long long)p95,
        (unsigned long long)min, ITER, HOST, buf);
}

typedef uint64_t (*hash_bytes_fn)(const void* buf, size_t len);
typedef uint64_t (*hash_u64_fn)(uint64_t n);

static uint64_t bench_hash_bytes_inner(hash_bytes_fn f, const void* buf,
                                       size_t len) {
    uint64_t t0 = now_ns();
    uint64_t acc = 0;
    for (int i = 0; i < ITER; ++i) {
        acc ^= f(buf, len);
    }
    sink_u64(acc);
    return (now_ns() - t0) / ITER;
}

static uint64_t bench_hash_u64_inner(hash_u64_fn f, uint64_t n) {
    uint64_t t0 = now_ns();
    uint64_t acc = 0;
    for (int i = 0; i < ITER; ++i) {
        acc ^= f(n);
    }
    sink_u64(acc);
    return (now_ns() - t0) / ITER;
}

static void bench_bytes(const char* competitor, const char* workload,
                        hash_bytes_fn f, const void* buf, size_t len) {
    uint64_t times[SAMPLES];
    for (int i = 0; i < SAMPLES; ++i)
        times[i] = bench_hash_bytes_inner(f, buf, len);
    qsort(times, SAMPLES, sizeof(uint64_t), cmp_u64);
    emit_json(competitor, workload, times[SAMPLES / 2],
              times[(SAMPLES * 95) / 100], times[0]);
}

static void bench_u64(const char* competitor, const char* workload,
                      hash_u64_fn f, uint64_t n) {
    uint64_t times[SAMPLES];
    for (int i = 0; i < SAMPLES; ++i) times[i] = bench_hash_u64_inner(f, n);
    qsort(times, SAMPLES, sizeof(uint64_t), cmp_u64);
    emit_json(competitor, workload, times[SAMPLES / 2],
              times[(SAMPLES * 95) / 100], times[0]);
}

// ---- competitor wrappers ----

static uint64_t hf_xxh3(const void* buf, size_t len) {
    return (uint64_t)XXH3_64bits(buf, len);
}
static uint64_t hf_wyhash(const void* buf, size_t len) {
    return wyhash(buf, len, 0, _wyp);
}
static uint64_t hf_xxh3_u64(uint64_t n) {
    return (uint64_t)XXH3_64bits(&n, sizeof(n));
}
static uint64_t hf_wyhash_u64(uint64_t n) {
    return wyhash(&n, sizeof(n), 0, _wyp);
}

int main(void) {
    uint8_t b8[8], b16[16], b64[64];
    for (int i = 0; i < 8; ++i) b8[i] = i;
    for (int i = 0; i < 16; ++i) b16[i] = i;
    for (int i = 0; i < 64; ++i) b64[i] = i;

    bench_bytes("xxhash (XXH3_64)", "hash_bytes_8B", hf_xxh3, b8, 8);
    bench_bytes("xxhash (XXH3_64)", "hash_bytes_16B", hf_xxh3, b16, 16);
    bench_bytes("xxhash (XXH3_64)", "hash_bytes_64B", hf_xxh3, b64, 64);

    bench_bytes("wyhash", "hash_bytes_8B", hf_wyhash, b8, 8);
    bench_bytes("wyhash", "hash_bytes_16B", hf_wyhash, b16, 16);
    bench_bytes("wyhash", "hash_bytes_64B", hf_wyhash, b64, 64);

    bench_u64("xxhash (XXH3_64)", "hash_u64", hf_xxh3_u64,
              0xdeadbeefcafebabeULL);
    bench_u64("wyhash", "hash_u64", hf_wyhash_u64, 0xdeadbeefcafebabeULL);

    if (g_sink == 0xDEADBEEF) printf("// sink\n");
    return 0;
}

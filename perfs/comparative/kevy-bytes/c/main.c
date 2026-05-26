// Cross-language counterpart of kevy-bytes' bench, C side: measures
// Redis SDS (Simple Dynamic Strings, antirez/sds) on the same clone /
// eq / from_bytes workloads. Emits one JSON line per measurement.
//
// SDS is a byte-string with a length-prefixed heap header; no SSO.
// The header type is chosen at construction time (sdshdr5/8/16/32/64)
// to minimise per-instance overhead for short strings, but the value
// itself always sits on the heap.

#define _POSIX_C_SOURCE 199309L
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "sds.h"

#define ITER     1000000
#define SAMPLES  25
#define HOST     "M4-Pro-aarch64"
#define STONE    "kevy-bytes"
#define LANG     "c"

static volatile uint64_t g_sink_u64 = 0;
static volatile int g_sink_int = 0;

static void __attribute__((noinline)) sink_sds(sds s) {
    g_sink_u64 ^= (uintptr_t)s;
    g_sink_u64 ^= sdslen(s);
}

static void __attribute__((noinline)) sink_int(int i) { g_sink_int ^= i; }

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static int cmp_u64(const void* a, const void* b) {
    uint64_t va = *(const uint64_t*)a, vb = *(const uint64_t*)b;
    return (va > vb) - (va < vb);
}

typedef void (*workload_fn)(void* ctx);

static void emit_json(const char* competitor, const char* workload,
                      uint64_t median, uint64_t p95, uint64_t min) {
    char buf[32];
    time_t t = time(NULL);
    strftime(buf, sizeof(buf), "%Y-%m-%dT%H:%M:%SZ", gmtime(&t));
    printf(
        "{\"stone\":\"%s\",\"language\":\"%s\",\"competitor\":\"%s\","
        "\"workload\":\"%s\",\"metric\":\"ns_per_op\","
        "\"value_median\":%llu,\"value_p95\":%llu,\"value_min\":%llu,"
        "\"iterations\":%d,\"host\":\"%s\",\"date\":\"%s\"}\n",
        STONE, LANG, competitor, workload,
        (unsigned long long)median, (unsigned long long)p95,
        (unsigned long long)min, ITER, HOST, buf);
}

static uint64_t time_one(workload_fn f, void* ctx) {
    uint64_t t0 = now_ns();
    for (int i = 0; i < ITER; ++i) f(ctx);
    uint64_t t1 = now_ns();
    return (t1 - t0) / ITER;
}

static void bench(const char* competitor, const char* workload,
                  workload_fn f, void* ctx) {
    uint64_t times[SAMPLES];
    for (int i = 0; i < SAMPLES; ++i) times[i] = time_one(f, ctx);
    qsort(times, SAMPLES, sizeof(uint64_t), cmp_u64);
    emit_json(competitor, workload, times[SAMPLES / 2],
              times[(SAMPLES * 95) / 100], times[0]);
}

// ---- workloads ----

typedef struct {
    sds src;
} clone_ctx;
static void wl_clone(void* p) {
    clone_ctx* c = (clone_ctx*)p;
    sds dup = sdsdup(c->src);
    sink_sds(dup);
    sdsfree(dup);
}

typedef struct {
    sds a, b;
} eq_ctx;
static void wl_eq(void* p) {
    eq_ctx* c = (eq_ctx*)p;
    volatile sds aa = c->a;
    volatile sds bb = c->b;
    sink_int(sdscmp((sds)aa, (sds)bb));
}

typedef struct {
    const char* buf;
    size_t len;
} from_ctx;
static void wl_from(void* p) {
    from_ctx* c = (from_ctx*)p;
    sds s = sdsnewlen(c->buf, c->len);
    sink_sds(s);
    sdsfree(s);
}

int main(void) {
    const char short_str[] = "hello world!"; // 12 bytes
    char long_buf[64];
    for (int i = 0; i < 64; ++i) long_buf[i] = (char)i;

    sds short_sds = sdsnewlen(short_str, 12);
    sds long_sds = sdsnewlen(long_buf, 64);

    // clone
    {
        clone_ctx c = {.src = short_sds};
        bench("sds", "clone_inline_12B", wl_clone, &c);
    }
    {
        clone_ctx c = {.src = long_sds};
        bench("sds", "clone_heap_64B", wl_clone, &c);
    }

    // eq
    {
        sds a = sdsdup(short_sds), b = sdsdup(short_sds);
        eq_ctx c = {.a = a, .b = b};
        bench("sds", "eq_inline_12B", wl_eq, &c);
        sdsfree(a);
        sdsfree(b);
    }
    {
        sds a = sdsdup(long_sds), b = sdsdup(long_sds);
        eq_ctx c = {.a = a, .b = b};
        bench("sds", "eq_heap_64B", wl_eq, &c);
        sdsfree(a);
        sdsfree(b);
    }

    // from_bytes
    {
        from_ctx c = {.buf = short_str, .len = 12};
        bench("sds", "from_bytes_inline_12B", wl_from, &c);
    }
    {
        from_ctx c = {.buf = long_buf, .len = 64};
        bench("sds", "from_bytes_heap_64B", wl_from, &c);
    }

    sdsfree(short_sds);
    sdsfree(long_sds);

    if (g_sink_u64 == 0xDEADBEEF) printf("// sink\n");
    if (g_sink_int == 0xDEAD) printf("// sink\n");
    return 0;
}

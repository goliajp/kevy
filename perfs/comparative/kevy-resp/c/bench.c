// hiredis RESP parser bench (C reference for kevy-resp).
//
// Same workload as `../rust/src/main.rs`:
// - parse_command_set_3args: "*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nvalue\r\n"
// - parse_reply_bulk_12B:    "$12\r\nhello world!\r\n"
//
// hiredis is symmetric on the parse side — `redisReader` parses RESP frames
// regardless of whether they're commands or replies. We reuse one
// `redisReader` per sample (the canonical client pattern: scratch buffer
// reuse) so the comparison matches kevy-resp's stateless `parse_command` /
// `parse_reply` (both allocate-internally-per-call, but the wire-buffer
// reuse is the meaningful baseline).

#include <hiredis/hiredis.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <sys/utsname.h>

#define ITER 1000000
#define SAMPLES 25

static const char SET_CMD[] =
    "*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nvalue\r\n";
static const char BULK_REPLY[] = "$12\r\nhello world!\r\n";

static int cmp_u64(const void *a, const void *b) {
    unsigned long x = *(const unsigned long *)a;
    unsigned long y = *(const unsigned long *)b;
    return (x < y) ? -1 : (x > y);
}

static void iso_now(char *buf, size_t n) {
    time_t t = time(NULL);
    struct tm tm;
    gmtime_r(&t, &tm);
    strftime(buf, n, "%Y-%m-%dT%H:%M:%SZ", &tm);
}

static void host_str(char *buf, size_t n) {
    struct utsname u;
    if (uname(&u) == 0) {
        snprintf(buf, n, "%s-%s", u.sysname, u.machine);
        for (char *p = buf; *p; p++) if (*p >= 'A' && *p <= 'Z') *p += 32;
    } else {
        snprintf(buf, n, "unknown");
    }
}

static void bench_frame(const char *frame, size_t frame_len, const char *workload) {
    unsigned long times[SAMPLES];

    for (int s = 0; s < SAMPLES; s++) {
        redisReader *r = redisReaderCreate();
        if (!r) { fprintf(stderr, "redisReaderCreate failed\n"); exit(1); }

        struct timespec t0, t1;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        for (int i = 0; i < ITER; i++) {
            void *reply = NULL;
            redisReaderFeed(r, frame, frame_len);
            redisReaderGetReply(r, &reply);
            if (reply) freeReplyObject(reply);
        }
        clock_gettime(CLOCK_MONOTONIC, &t1);
        unsigned long ns = (unsigned long)(t1.tv_sec - t0.tv_sec) * 1000000000UL
                         + (unsigned long)(t1.tv_nsec - t0.tv_nsec);
        times[s] = ns / (unsigned long)ITER;
        redisReaderFree(r);
    }

    qsort(times, SAMPLES, sizeof(unsigned long), cmp_u64);
    unsigned long med = times[SAMPLES / 2];
    unsigned long p95 = times[(SAMPLES * 95) / 100];
    unsigned long min = times[0];

    char date_buf[32], host_buf[64];
    iso_now(date_buf, sizeof(date_buf));
    host_str(host_buf, sizeof(host_buf));

    printf("{\"stone\":\"kevy-resp\",\"language\":\"c\",\"competitor\":\"hiredis\","
           "\"workload\":\"%s\",\"metric\":\"ns_per_op\","
           "\"value_median\":%lu,\"value_p95\":%lu,\"value_min\":%lu,"
           "\"iterations\":%d,\"host\":\"%s\",\"date\":\"%s\"}\n",
           workload, med, p95, min, ITER, host_buf, date_buf);
}

int main(void) {
    bench_frame(SET_CMD, sizeof(SET_CMD) - 1, "parse_command_set_3args");
    bench_frame(BULK_REPLY, sizeof(BULK_REPLY) - 1, "parse_reply_bulk_12B");
    return 0;
}

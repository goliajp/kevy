// liburing nop round-trip latency bench (C reference for kevy-uring).
//
// Emits JSON-line output matching the schema in perfs/comparative/README.md.
// Build: see Makefile. Run: ./bench (no args).

#include <liburing.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <sys/utsname.h>

#define SAMPLES 25
#define INNER 100000

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
        // lowercase the sysname to match the rust-side "linux-x86_64" form
        for (char *p = buf; *p; p++) if (*p >= 'A' && *p <= 'Z') *p += 32;
    } else {
        snprintf(buf, n, "unknown");
    }
}

int main(void) {
    struct io_uring ring;
    if (io_uring_queue_init(32, &ring, 0) != 0) {
        fprintf(stderr, "io_uring_queue_init failed\n");
        return 1;
    }

    unsigned long times[SAMPLES];

    for (int s = 0; s < SAMPLES; s++) {
        struct timespec t0, t1;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        for (int i = 0; i < INNER; i++) {
            struct io_uring_sqe *sqe = io_uring_get_sqe(&ring);
            io_uring_prep_nop(sqe);
            io_uring_submit_and_wait(&ring, 1);
            struct io_uring_cqe *cqe;
            io_uring_wait_cqe(&ring, &cqe);
            io_uring_cqe_seen(&ring, cqe);
        }
        clock_gettime(CLOCK_MONOTONIC, &t1);
        unsigned long ns = (unsigned long)(t1.tv_sec - t0.tv_sec) * 1000000000UL
                         + (unsigned long)(t1.tv_nsec - t0.tv_nsec);
        times[s] = ns / (unsigned long)INNER;
    }

    qsort(times, SAMPLES, sizeof(unsigned long), cmp_u64);
    unsigned long med = times[SAMPLES / 2];
    unsigned long p95 = times[(SAMPLES * 95) / 100];
    unsigned long min = times[0];

    char date_buf[32], host_buf[64];
    iso_now(date_buf, sizeof(date_buf));
    host_str(host_buf, sizeof(host_buf));

    printf("{\"stone\":\"kevy-uring\",\"language\":\"c\",\"competitor\":\"liburing\","
           "\"workload\":\"nop_rtt\",\"metric\":\"ns_per_op\","
           "\"value_median\":%lu,\"value_p95\":%lu,\"value_min\":%lu,"
           "\"iterations\":%d,\"host\":\"%s\",\"date\":\"%s\"}\n",
           med, p95, min, INNER, host_buf, date_buf);

    io_uring_queue_exit(&ring);
    return 0;
}

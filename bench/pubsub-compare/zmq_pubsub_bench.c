/* zmq_pubsub_bench.c — 1 PUB → N SUB fan-out throughput, matching
 * the shape of kevy's bench/pubsub_loopback.sh.
 *
 * Parameters (envvar, all optional):
 *   SUBS    number of subscriber sockets   (default 50)
 *   MSGS    publish count                   (default 200000)
 *   SIZE    bytes per message               (default 16)
 *   ENDPOINT  pub bind / sub connect       (default tcp://127.0.0.1:5556)
 *   USE_INPROC  if 1, uses inproc://kevy-zmq-bench  (no syscalls)
 *
 * Reports: delivered msg/s (publishes × SUBS / elapsed) AND publish rate.
 *
 * Build:  gcc -O2 -o /tmp/zmq_pubsub_bench /tmp/zmq_pubsub_bench.c -lzmq -lpthread
 * Run :   SUBS=50 MSGS=200000 SIZE=16 /tmp/zmq_pubsub_bench
 */
#define _POSIX_C_SOURCE 200809L
#define _DEFAULT_SOURCE 1
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <time.h>
#include <unistd.h>
#include <stdatomic.h>
#include <zmq.h>

static void *ctx;
static int   subs_cnt;
static int   msgs_cnt;
static int   msg_size;
static const char *endpoint;
static int   use_inproc;

static atomic_long delivered;
static atomic_int  ready_subs;
static pthread_barrier_t go;

static double now_secs(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

static void *sub_thread(void *arg) {
  (void)arg;
  void *s = zmq_socket(ctx, ZMQ_SUB);
  if (!s) { perror("zmq_socket SUB"); pthread_exit(NULL); }
  if (zmq_setsockopt(s, ZMQ_SUBSCRIBE, "", 0) != 0) {
    perror("setsockopt SUBSCRIBE"); zmq_close(s); pthread_exit(NULL);
  }
  /* deeper RX buffer so a fast PUB doesn't drop on conflate */
  int hwm = 1 << 20;
  zmq_setsockopt(s, ZMQ_RCVHWM, &hwm, sizeof(hwm));
  if (zmq_connect(s, endpoint) != 0) {
    perror("zmq_connect"); zmq_close(s); pthread_exit(NULL);
  }
  atomic_fetch_add(&ready_subs, 1);
  pthread_barrier_wait(&go);

  char buf[4096];
  long got = 0;
  for (long i = 0; i < (long)msgs_cnt; i++) {
    int n = zmq_recv(s, buf, sizeof(buf), 0);
    if (n < 0) { perror("zmq_recv"); break; }
    got++;
  }
  atomic_fetch_add(&delivered, got);
  zmq_close(s);
  return NULL;
}

int main(void) {
  subs_cnt  = getenv("SUBS")     ? atoi(getenv("SUBS"))   : 50;
  msgs_cnt  = getenv("MSGS")     ? atoi(getenv("MSGS"))   : 200000;
  msg_size  = getenv("SIZE")     ? atoi(getenv("SIZE"))   : 16;
  endpoint  = getenv("ENDPOINT") ? getenv("ENDPOINT")     : "tcp://127.0.0.1:5556";
  use_inproc = getenv("USE_INPROC") && getenv("USE_INPROC")[0]=='1';
  if (use_inproc) endpoint = "inproc://kevy-zmq-bench";

  ctx = zmq_ctx_new();
  zmq_ctx_set(ctx, ZMQ_IO_THREADS, use_inproc ? 0 : 1);

  void *pub = zmq_socket(ctx, ZMQ_PUB);
  int hwm = 1 << 20;
  zmq_setsockopt(pub, ZMQ_SNDHWM, &hwm, sizeof(hwm));
  if (zmq_bind(pub, endpoint) != 0) { perror("zmq_bind"); return 1; }

  atomic_init(&delivered, 0);
  atomic_init(&ready_subs, 0);
  pthread_barrier_init(&go, NULL, subs_cnt + 1);

  pthread_t *threads = calloc(subs_cnt, sizeof(*threads));
  for (int i = 0; i < subs_cnt; i++) {
    if (pthread_create(&threads[i], NULL, sub_thread, NULL) != 0) {
      perror("pthread_create"); return 1;
    }
  }

  /* Wait for all subscribers to connect (PUB has no handshake; we use a
     synchronous barrier here, then a brief sleep to let TCP/zmq settle). */
  while (atomic_load(&ready_subs) < subs_cnt) usleep(1000);
  usleep(50000); /* settle */
  pthread_barrier_wait(&go);

  char *payload = calloc(1, msg_size);
  memset(payload, 'x', msg_size);
  double t0 = now_secs();
  for (long i = 0; i < (long)msgs_cnt; i++) {
    if (zmq_send(pub, payload, msg_size, 0) < 0) {
      perror("zmq_send"); break;
    }
  }
  double t_pub = now_secs() - t0;

  /* Wait for subs to drain. */
  for (int i = 0; i < subs_cnt; i++) pthread_join(threads[i], NULL);
  double t_total = now_secs() - t0;

  long delivered_total = atomic_load(&delivered);
  printf("zmq-pubsub endpoint=%s subs=%d msgs=%d size=%dB "
         "delivered=%ld msg/s publishes=%.0f/s elapsed=%.3fs pub_elapsed=%.3fs\n",
         endpoint, subs_cnt, msgs_cnt, msg_size,
         t_total > 0 ? (long)(delivered_total / t_total) : 0,
         t_pub   > 0 ? msgs_cnt / t_pub : 0,
         t_total, t_pub);

  free(payload);
  free(threads);
  zmq_close(pub);
  zmq_ctx_destroy(ctx);
  return 0;
}

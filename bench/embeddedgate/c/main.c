/* embeddedgate — C track. kevy C ABI scalar (kevy_get_shared zero-copy lane /
 * kevy_set) vs LMDB (mdb_get/mdb_put), the read-latency leader — the toughest,
 * most honorable bar (Symas + Mozilla corroborate LMDB's read dominance).
 *
 * Both reads are zero-copy views: LMDB mdb_get returns a pointer into the mmap
 * (valid during the txn); kevy_get_shared returns an Arc-clone view (freed with
 * kevy_buf_free_shared). Fair apples-to-apples on the read shape.
 *
 * LMDB forces an explicit transaction; kevy's scalar has none — so we report
 * BOTH cold-single-op (txn-per-op) and amortized (one txn/N). kevy's number is
 * the same in both. Headline tier T-async: kevy AOF EverySec, LMDB MDB_NOSYNC
 * (OS-flush, no per-op fsync). k/p < 1 means kevy faster.
 *
 * Relative standing from the dev host; definitive SLA = lx64 (perf §9).
 * Build/run via ./run.sh.
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#include "kevy.h"
#include "lmdb.h"

#define N 100000
#define KEYS 200
#define RUNS 3
static const size_t sizes[] = {16, 256, 4096, 65536};
#define NSIZES 4

static char keybuf[KEYS][16];
static size_t keylen[KEYS];

static void mkkeys(void) {
  for (int i = 0; i < KEYS; i++) keylen[i] = snprintf(keybuf[i], sizeof keybuf[i], "k:%d", i);
}

static double nowns(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return (double)t.tv_sec * 1e9 + (double)t.tv_nsec;
}

static int cmpd(const void *a, const void *b) {
  double x = *(const double *)a, y = *(const double *)b;
  return (x > y) - (x < y);
}

/* median-of-RUNS ns/op. Variadic so the body's brace-initializer commas
 * (MDB_val k = {..}, d = {..}) aren't parsed as macro-argument separators. */
#define TIME(dst, ...)                              \
  do {                                              \
    double s[RUNS];                                 \
    for (int r = 0; r < RUNS; r++) {                \
      double t0 = nowns();                          \
      __VA_ARGS__;                                  \
      s[r] = (nowns() - t0) / (double)N;            \
    }                                               \
    qsort(s, RUNS, sizeof(double), cmpd);           \
    dst = s[1];                                     \
  } while (0)

typedef struct {
  double get_cold, get_amort, set_cold, set_amort;
} res;

/* ---- kevy: bare scalar, no txn (cold == amortized) ---- */
static res bench_kevy(KevyDb *db, const uint8_t *val, size_t vlen) {
  for (int i = 0; i < KEYS; i++) kevy_set(db, (uint8_t *)keybuf[i], keylen[i], val, vlen, 0);
  res r;
  TIME(r.get_cold, {
    for (int i = 0; i < N; i++) {
      KevyBuf b;
      int k = i % KEYS;
      if (kevy_get_shared(db, (uint8_t *)keybuf[k], keylen[k], &b) == 1)
        kevy_buf_free_shared(b.ptr, b.len, b.cap);
    }
  });
  r.get_amort = r.get_cold; /* kevy has no read txn to amortize */
  TIME(r.set_cold, {
    for (int i = 0; i < N; i++) {
      int k = i % KEYS;
      kevy_set(db, (uint8_t *)keybuf[k], keylen[k], val, vlen, 0);
    }
  });
  /* SET amortized: kevy_set_many — one crossing, N sets (the batch path) */
  const uint8_t **mk = malloc(N * sizeof(uint8_t *));
  size_t *mkl = malloc(N * sizeof(size_t));
  const uint8_t **mv = malloc(N * sizeof(uint8_t *));
  size_t *mvl = malloc(N * sizeof(size_t));
  for (int i = 0; i < N; i++) {
    int k = i % KEYS;
    mk[i] = (const uint8_t *)keybuf[k];
    mkl[i] = keylen[k];
    mv[i] = val;
    mvl[i] = vlen;
  }
  TIME(r.set_amort, { kevy_set_many(db, N, mk, mkl, mv, mvl); });
  free(mk);
  free(mkl);
  free(mv);
  free(mvl);
  return r;
}

/* ---- LMDB: mdb_get/mdb_put inside txns ---- */
static res bench_lmdb(MDB_env *env, const uint8_t *val, size_t vlen) {
  MDB_dbi dbi;
  MDB_txn *txn;
  /* seed + open dbi */
  mdb_txn_begin(env, NULL, 0, &txn);
  mdb_dbi_open(txn, NULL, 0, &dbi);
  for (int i = 0; i < KEYS; i++) {
    MDB_val k = {keylen[i], keybuf[i]}, d = {vlen, (void *)val};
    mdb_put(txn, dbi, &k, &d, 0);
  }
  mdb_txn_commit(txn);

  res r;
  /* GET cold: one rdonly txn per get (what a one-off get costs) */
  TIME(r.get_cold, {
    for (int i = 0; i < N; i++) {
      int idx = i % KEYS;
      MDB_txn *t;
      MDB_val k = {keylen[idx], keybuf[idx]}, d;
      mdb_txn_begin(env, NULL, MDB_RDONLY, &t);
      mdb_get(t, dbi, &k, &d);
      mdb_txn_abort(t);
    }
  });
  /* GET amortized: one rdonly txn reused for all N gets (tight read loop) */
  {
    MDB_txn *rt;
    mdb_txn_begin(env, NULL, MDB_RDONLY, &rt);
    TIME(r.get_amort, {
      for (int i = 0; i < N; i++) {
        int idx = i % KEYS;
        MDB_val k = {keylen[idx], keybuf[idx]}, d;
        mdb_get(rt, dbi, &k, &d);
      }
    });
    mdb_txn_abort(rt);
  }
  /* SET cold: begin+put+commit per op */
  TIME(r.set_cold, {
    for (int i = 0; i < N; i++) {
      int idx = i % KEYS;
      MDB_txn *t;
      MDB_val k = {keylen[idx], keybuf[idx]}, d = {vlen, (void *)val};
      mdb_txn_begin(env, NULL, 0, &t);
      mdb_put(t, dbi, &k, &d, 0);
      mdb_txn_commit(t);
    }
  });
  /* SET amortized: one txn wrapping N puts */
  TIME(r.set_amort, {
    MDB_txn *t;
    mdb_txn_begin(env, NULL, 0, &t);
    for (int i = 0; i < N; i++) {
      int idx = i % KEYS;
      MDB_val k = {keylen[idx], keybuf[idx]}, d = {vlen, (void *)val};
      mdb_put(t, dbi, &k, &d, 0);
    }
    mdb_txn_commit(t);
  });
  return r;
}

static const char *verdict(double k, double p) {
  static char buf[32];
  double r = k / p;
  if (r < 0.97) snprintf(buf, sizeof buf, "kevy %.2fx", p / k);
  else if (r > 1.03) snprintf(buf, sizeof buf, "peer %.2fx", k / p);
  else snprintf(buf, sizeof buf, "tie");
  return buf;
}

static void table(const char *field, double *kevy, double *peer) {
  printf("\n### %s\n", field);
  printf("|   size | kevy ns | lmdb ns | k/p | verdict |\n");
  printf("|-------:|--------:|--------:|----:|---------|\n");
  for (int i = 0; i < NSIZES; i++)
    printf("| %6zu | %7.0f | %7.0f | %4.2f | %s |\n", sizes[i], kevy[i], peer[i], kevy[i] / peer[i],
           verdict(kevy[i], peer[i]));
}

int main(void) {
  mkkeys();
  char tmpl[] = "/tmp/embgate-c-XXXXXX";
  char *root = mkdtemp(tmpl);

  printf("# embeddedgate — C — kevy C ABI scalar vs LMDB %d.%d.%d\n", MDB_VERSION_MAJOR,
         MDB_VERSION_MINOR, MDB_VERSION_PATCH);
  printf("N=%d ops, %d warm keys, median-of-%d, sizes 16/256/4096/65536 B\n", N, KEYS, RUNS);
  printf("kevy: cold==amortized (no txn). LMDB: cold=txn/op, amort=one txn/N.\n");
  printf("\n## T-async — kevy AOF EverySec / LMDB MDB_NOSYNC (OS-flush, no per-op fsync)\n");

  double kgc[NSIZES], kga[NSIZES], ksc[NSIZES], ksa[NSIZES];
  double lgc[NSIZES], lga[NSIZES], lsc[NSIZES], lsa[NSIZES];

  for (int i = 0; i < NSIZES; i++) {
    size_t vlen = sizes[i];
    uint8_t *val = malloc(vlen);
    memset(val, 0x61, vlen);

    char kd[256];
    snprintf(kd, sizeof kd, "%s/kevy-%zu", root, vlen);
    mkdir(kd, 0755);
    KevyDb *kdb = kevy_open((uint8_t *)kd, strlen(kd));
    res kr = bench_kevy(kdb, val, vlen);
    kevy_close(kdb);
    kgc[i] = kr.get_cold; kga[i] = kr.get_amort; ksc[i] = kr.set_cold; ksa[i] = kr.set_amort;

    char ld[256];
    snprintf(ld, sizeof ld, "%s/lmdb-%zu", root, vlen);
    mkdir(ld, 0755);
    MDB_env *env;
    mdb_env_create(&env);
    mdb_env_set_mapsize(env, (size_t)1 << 30); /* 1 GiB */
    mdb_env_open(env, ld, MDB_NOSYNC, 0664);
    res lr = bench_lmdb(env, val, vlen);
    mdb_env_close(env);
    lgc[i] = lr.get_cold; lga[i] = lr.get_amort; lsc[i] = lr.set_cold; lsa[i] = lr.set_amort;

    free(val);
  }

  table("GET cold-1op", kgc, lgc);
  table("GET amortized", kga, lga);
  table("SET cold-1op", ksc, lsc);
  table("SET amortized", ksa, lsa);
  printf("\n(relative standing — dev host; definitive SLA = lx64 per perf §9)\n");
  return 0;
}

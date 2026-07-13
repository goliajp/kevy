/* The C smoke: what ffigate runs on every push. Opens a persistent store,
 * exercises the command entry, round-trips pub/sub, then reopens the same
 * directory and proves the data survived. Exits non-zero on the first lie.
 *
 * Build (from the repo root):
 *   cargo build -p kevy-ffi
 *   cc -Icrates/kevy-ffi/include crates/kevy-ffi/examples/c/smoke.c \
 *      target/debug/libkevy_ffi.a -o /tmp/kevy-smoke
 *   /tmp/kevy-smoke /tmp/kevy-smoke-data
 */
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "kevy.h"

/* Run one command from C string literals; returns the reply buffer. */
static KevyBuf cmd(KevyDb *db, size_t argc, const char **argv) {
  const uint8_t *ptrs[8];
  size_t lens[8];
  assert(argc <= 8);
  for (size_t i = 0; i < argc; i++) {
    ptrs[i] = (const uint8_t *)argv[i];
    lens[i] = strlen(argv[i]);
  }
  KevyBuf out;
  int32_t rc = kevy_cmd(db, argc, ptrs, lens, &out);
  assert(rc == 0);
  return out;
}

static void expect(KevyBuf got, const char *want, const char *what) {
  if (got.len != strlen(want) || memcmp(got.ptr, want, got.len) != 0) {
    fprintf(stderr, "FAIL %s: got %.*s", what, (int)got.len, got.ptr);
    /* exit without freeing — we are already failing */
    _Exit(1);
  }
  kevy_buf_free(got.ptr, got.len, got.cap);
}

int main(int argc, char **argv) {
  assert(argc == 2); /* data dir */
  assert(kevy_abi() == KEVY_ABI);
  printf("kevy %s / abi %u\n", kevy_version(), kevy_abi());

  KevyDb *db = kevy_open((const uint8_t *)argv[1], strlen(argv[1]));
  assert(db != NULL);

  const char *set[] = {"SET", "smoke:k", "v1"};
  expect(cmd(db, 3, set), "+OK\r\n", "SET");

  const char *get[] = {"GET", "smoke:k"};
  expect(cmd(db, 2, get), "$2\r\nv1\r\n", "GET");

  const char *bad[] = {"NOSUCHVERB"};
  KevyBuf err = cmd(db, 1, bad);
  assert(err.len > 0 && err.ptr[0] == '-'); /* protocol error, rc still 0 */
  kevy_buf_free(err.ptr, err.len, err.cap);

  /* pub/sub round trip: ack frame, then the message frame */
  KevySub *sub = kevy_subscribe(db, (const uint8_t *)"c1", 2);
  assert(sub != NULL);
  KevyBuf frame;
  assert(kevy_sub_next(sub, &frame) == 1); /* subscribe ack */
  kevy_buf_free(frame.ptr, frame.len, frame.cap);

  const char *pub[] = {"PUBLISH", "c1", "hello"};
  expect(cmd(db, 3, pub), ":1\r\n", "PUBLISH");
  assert(kevy_sub_next(sub, &frame) == 1);
  expect(frame, "*3\r\n$7\r\nmessage\r\n$2\r\nc1\r\n$5\r\nhello\r\n", "frame");
  assert(kevy_sub_next(sub, &frame) == 0); /* drained */
  kevy_sub_close(sub);

  /* durability: close, reopen the same dir, the key is still there */
  kevy_close(db);
  db = kevy_open((const uint8_t *)argv[1], strlen(argv[1]));
  assert(db != NULL);
  expect(cmd(db, 2, get), "$2\r\nv1\r\n", "GET after reopen");

  /* scalar fast path round trip */
  assert(kevy_set(db, (const uint8_t *)"fast:k", 6, (const uint8_t *)"fv", 2, 0) == 0);
  KevyBuf fv;
  assert(kevy_get(db, (const uint8_t *)"fast:k", 6, &fv) == 1);
  assert(fv.len == 2 && memcmp(fv.ptr, "fv", 2) == 0);
  kevy_buf_free(fv.ptr, fv.len, fv.cap);
  assert(kevy_get(db, (const uint8_t *)"fast:none", 9, &fv) == 0);

  const char *del[] = {"DEL", "smoke:k"};
  expect(cmd(db, 2, del), ":1\r\n", "DEL");
  kevy_close(db);

  puts("smoke: ok");
  return 0;
}

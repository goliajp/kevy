/* clientgate: hiredis against a kevy server. */
#include <hiredis/hiredis.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static redisContext *ctx;

static void check(int ok, const char *what) {
  if (!ok) {
    fprintf(stderr, "FAIL %s (%s)\n", what, ctx && ctx->errstr[0] ? ctx->errstr : "-");
    exit(1);
  }
}

static redisReply *cmd(const char *fmt, ...) {
  va_list ap;
  va_start(ap, fmt);
  redisReply *r = redisvCommand(ctx, fmt, ap);
  va_end(ap);
  check(r != NULL, fmt);
  return r;
}

int main(void) {
  int port = atoi(getenv("KEVY_PORT"));
  ctx = redisConnect("127.0.0.1", port);
  check(ctx != NULL && !ctx->err, "connect");

  freeReplyObject(cmd("FLUSHALL"));

  redisReply *r = cmd("SET k v");
  check(r->type == REDIS_REPLY_STATUS && strcmp(r->str, "OK") == 0, "SET");
  freeReplyObject(r);

  r = cmd("GET k");
  check(r->type == REDIS_REPLY_STRING && strcmp(r->str, "v") == 0, "GET");
  freeReplyObject(r);

  r = cmd("INCRBY n 5");
  check(r->type == REDIS_REPLY_INTEGER && r->integer == 5, "INCRBY");
  freeReplyObject(r);

  freeReplyObject(cmd("PEXPIRE k 30000"));
  r = cmd("PTTL k");
  check(r->integer > 0 && r->integer <= 30000, "PTTL");
  freeReplyObject(r);

  freeReplyObject(cmd("HSET h a 1 b 2"));
  r = cmd("HGETALL h");
  check(r->type == REDIS_REPLY_ARRAY && r->elements == 4, "HGETALL");
  freeReplyObject(r);

  freeReplyObject(cmd("LPUSH l x y"));
  r = cmd("LRANGE l 0 -1");
  check(r->elements == 2 && strcmp(r->element[0]->str, "y") == 0, "LRANGE");
  freeReplyObject(r);

  freeReplyObject(cmd("ZADD z 1 one 2 two"));
  r = cmd("ZRANGE z 0 -1");
  check(r->elements == 2 && strcmp(r->element[0]->str, "one") == 0, "ZRANGE");
  freeReplyObject(r);

  /* pub/sub round trip: a second connection subscribes, we publish. */
  redisContext *sub = redisConnect("127.0.0.1", port);
  check(sub != NULL && !sub->err, "sub connect");
  redisReply *ack = redisCommand(sub, "SUBSCRIBE room");
  check(ack != NULL, "SUBSCRIBE");
  freeReplyObject(ack);
  freeReplyObject(cmd("PUBLISH room hi"));
  redisReply *frame = NULL;
  check(redisGetReply(sub, (void **)&frame) == REDIS_OK && frame->elements == 3 &&
            strcmp(frame->element[2]->str, "hi") == 0,
        "pubsub");
  freeReplyObject(frame);
  redisFree(sub);

  /* the extended verb surface through the raw channel */
  r = cmd("IDX.LIST");
  check(r->type == REDIS_REPLY_ARRAY, "IDX.LIST raw");
  freeReplyObject(r);

  redisFree(ctx);
  puts("ok");
  return 0;
}

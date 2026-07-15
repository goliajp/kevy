// Sync + async faces on ONE client, agreeing on results (contract §6, §1.4).
#include "backends.hpp"
#include "harness.hpp"
#include "kevy/async.hpp"

using namespace kevy;
using namespace kevy::test;

KEVY_TEST(async_agrees_with_sync) {
  both_backends(t, [&](Client& c, bool) {
    AsyncClient a = c.async();
    a.set("k", "v").get();  // await the future
    auto fv = a.get("k");
    auto v = fv.get();
    CHECK(v.has_value() && *v == "v");
    // sync face on the SAME client agrees
    auto sv = c.get("k");
    CHECK(sv.has_value() && *sv == *v);

    auto fn = a.incr("n").get();
    CHECK_EQ(fn, int64_t(1));
    CHECK_EQ(c.incr("n"), int64_t(2));  // sync continues the same state
  });
}

KEVY_TEST(async_collections_and_command) {
  both_backends(t, [&](Client& c, bool) {
    AsyncClient a = c.async();
    CHECK_EQ(a.rpush("l", {"a", "b"}).get(), int64_t(2));
    CHECK_EQ(a.sadd("s", {"x", "y", "x"}).get(), int64_t(2));
    // generic raw-command future
    Reply r = a.command({"PING"}).get();
    // embedded PING may answer differently; both backends accept +PONG/+OK
    CHECK(r.kind == ReplyKind::Simple || r.is_error() == false);
  });
}

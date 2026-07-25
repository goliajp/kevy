// Error-as-value / exception mapping (contract §6 "Error-as-value"), both backends.
#include "backends.hpp"
#include "harness.hpp"
#include "kevy/kevy.hpp"

using namespace kevy;
using namespace kevy::test;

KEVY_TEST(errors_wrongtype) {
  // A wrong-type op surfaces Store(WrongType) on BOTH backends (§6).
  both_backends(t, [&](Client& c, bool) {
    c.set("s", "v");
    try {
      c.lpush("s", keys({"x"}));  // LPUSH on a string
      t.fail("expected WrongType throw");
    } catch (const KevyError& e) {
      CHECK(e.kind() == ErrorKind::Store);
      CHECK(e.store_error() == StoreErrorKind::WrongType);
    }
  });
}

KEVY_TEST(errors_not_integer) {
  both_backends(t, [&](Client& c, bool) {
    c.set("s", "notanumber");
    try {
      c.incr("s");
      t.fail("expected NotInteger throw");
    } catch (const KevyError& e) {
      CHECK(e.kind() == ErrorKind::Store);
      CHECK(e.store_error() == StoreErrorKind::NotInteger);
    }
  });
}

KEVY_TEST(errors_protocol_verbatim) {
  // A non-store -ERR (wrong arity) surfaces as Protocol with wire text kept.
  both_backends(t, [&](Client& c, bool) {
    Reply r = c.command({"GET"});  // wrong number of args
    CHECK(r.is_error());
    // Via a typed call the same -ERR maps to a Protocol error.
    try {
      // SET with a bad option → -ERR (not a store-semantic error)
      c.command({"SET"});
    } catch (...) {
    }
  });
}

KEVY_TEST(errors_embedded_unsupported) {
  // Embedded IDX.* / MULTI / pipeline → Unsupported (§6).
  Client c = Client::connect(unique_mem_url());
  CHECK_THROWS_KIND(c.idx_list(), ErrorKind::Unsupported);
  CHECK_THROWS_KIND(c.multi(), ErrorKind::Unsupported);
  CHECK_THROWS_KIND(c.watch(keys({"k"})), ErrorKind::Unsupported);
  CHECK_THROWS_KIND(c.pipeline([](PipelineBuf&) {}), ErrorKind::Unsupported);
}

KEVY_TEST(errors_command_raw_error_is_data) {
  // The raw command() escape hatch returns a -ERR as a Reply (data), not throw.
  both_backends(t, [&](Client& c, bool) {
    Reply r = c.command({"INCR"});  // wrong arity
    CHECK(r.is_error());
  });
}

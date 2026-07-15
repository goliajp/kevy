// Embedded store contract + persistence (contract §6 "Embedded store contract").
#include <unistd.h>

#include <cstdio>
#include <cstdlib>
#include <string>

#include "harness.hpp"
#include "kevy/embedded.hpp"
#include "kevy/reply.hpp"

using namespace kevy;
using namespace kevy::test;

KEVY_TEST(embedded_cmd_scalar) {
  EmbeddedStore db = EmbeddedStore::open_mem();
  // cmd(argv) runs any verb and returns a parseable RESP reply.
  Reply r = db.cmd({"SET", "k", "v"});
  CHECK(r.kind == ReplyKind::Simple && r.str() == "OK");
  Reply g = db.cmd({"GET", "k"});
  CHECK(g.kind == ReplyKind::Bulk && g.str() == "v");
  // scalar fast paths
  db.set("s", "fast", 0);
  auto v = db.get("s");
  CHECK(v.has_value() && *v == "fast");
  CHECK(!db.get("missing").has_value());
}

KEVY_TEST(embedded_subscribe_poll_and_wait) {
  EmbeddedStore db = EmbeddedStore::open_mem();
  Subscription sub = db.subscribe("room");
  db.cmd({"PUBLISH", "room", "hello"});
  // Block/poll for frames until the message arrives (ack frames may precede).
  bool got = false;
  for (int i = 0; i < 20 && !got; i++) {
    auto frame = sub.wait(500);  // block up to 500ms
    if (!frame) break;           // timeout
    const Reply& r = *frame;
    if (r.kind == ReplyKind::Array && r.array.size() == 3 && r.array[0].str() == "message") {
      CHECK_EQ(r.array[2].str(), std::string("hello"));
      got = true;
    }
  }
  CHECK(got);
}

KEVY_TEST(embedded_persistence_survives_reopen) {
  // Write, close, reopen same dir → state survives (snapshot + AOF replay).
  char tmpl[] = "/tmp/kevy-cpp-persist-XXXXXX";
  char* dir = ::mkdtemp(tmpl);
  CHECK(dir != nullptr);
  if (dir == nullptr) return;
  std::string path = dir;
  {
    EmbeddedStore db = EmbeddedStore::open(path);
    db.cmd({"SET", "persisted", "yes"});
    db.close();  // flushes on close
  }
  {
    EmbeddedStore db2 = EmbeddedStore::open(path);
    auto v = db2.get("persisted");
    CHECK(v.has_value());
    CHECK_EQ(v.value_or(std::string()), std::string("yes"));
  }
}

KEVY_TEST(embedded_version_abi) {
  CHECK(!EmbeddedStore::version().empty());
  CHECK(EmbeddedStore::abi() >= 1);
}

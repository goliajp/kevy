// Embedded store contract + persistence (contract §6 "Embedded store contract").
#include <unistd.h>

#include <cstdio>
#include <cstdlib>
#include <string>
#include <string_view>
#include <utility>

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

KEVY_TEST(embedded_get_view_zero_copy) {
  EmbeddedStore db = EmbeddedStore::open_mem();
  db.set("k", "hello");
  // Zero-copy view over the shared lane.
  auto sv = db.get_view("k");
  CHECK(sv.has_value());
  CHECK(sv->view() == std::string_view("hello"));
  CHECK_EQ(sv->size(), size_t(5));
  CHECK(!sv->empty());
  CHECK_EQ(sv->bytes().size(), size_t(5));
  // Move leaves the source empty and keeps the value valid in the destination.
  SharedValue moved = std::move(*sv);
  CHECK(moved.view() == std::string_view("hello"));
  // Miss → nullopt.
  CHECK(!db.get_view("nope").has_value());
  // Empty value: a present hit whose view() is empty (not a dangling range),
  // and the paired shared free still runs — no UB on ptr==null,len==0 (§8).
  db.set("e", "");
  auto ev = db.get_view("e");
  CHECK(ev.has_value());
  CHECK(ev->empty());
  CHECK(ev->view() == std::string_view(""));
  auto g = db.get("e");  // copying convenience over the same lane
  CHECK(g.has_value() && g->empty());
}

KEVY_TEST(embedded_get_wrongtype_is_typed) {
  // The high-level GET rides the zero-copy scalar lane, which rejects a
  // non-string key. That must surface as a typed Store(WrongType), NOT the
  // opaque "misuse" ProtocolError — matching the framed cmd path and Client.
  EmbeddedStore db = EmbeddedStore::open_mem();
  db.cmd({"RPUSH", "list", "a", "b"});  // a list-typed key
  // get_view path
  try {
    db.get_view("list");
    t.fail("expected WrongType throw from get_view");
  } catch (const KevyError& e) {
    CHECK(e.kind() == ErrorKind::Store);
    CHECK(e.store_error() == StoreErrorKind::WrongType);
  }
  // copying get() path (built on get_view)
  try {
    db.get("list");
    t.fail("expected WrongType throw from get");
  } catch (const KevyError& e) {
    CHECK(e.kind() == ErrorKind::Store);
    CHECK(e.store_error() == StoreErrorKind::WrongType);
  }
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

// Connection & URL routing (contract §6, first block).
#include "backends.hpp"
#include "harness.hpp"
#include "kevy/client.hpp"

using namespace kevy;
using namespace kevy::test;

KEVY_TEST(url_mem_anon_isolated) {
  // Two anonymous mem:// are independent stores.
  Client a = Client::connect("mem://");
  Client b = Client::connect("mem://");
  a.set("k", "va");
  CHECK(!b.get("k").has_value());
  a.close();
  b.close();
}

KEVY_TEST(url_mem_named_shared) {
  // Same mem://<name> opened twice shares one store.
  std::string url = "mem://shared-" + std::to_string(42);
  Client a = Client::connect(url);
  Client b = Client::connect(url);
  a.set("k", "shared-val");
  auto v = b.get("k");
  CHECK(v.has_value());
  CHECK_EQ(*v, std::string("shared-val"));
}

KEVY_TEST(url_reject_tls) {
  CHECK_THROWS_KIND(Client::connect("rediss://localhost"), ErrorKind::Unsupported);
  CHECK_THROWS_KIND(Client::connect("kevys://localhost"), ErrorKind::Unsupported);
}

KEVY_TEST(url_reject_auth) {
  CHECK_THROWS_KIND(Client::connect("redis://user:pass@localhost"), ErrorKind::Unsupported);
}

KEVY_TEST(url_reject_unknown_scheme) {
  CHECK_THROWS_KIND(Client::connect("wat://localhost"), ErrorKind::InvalidInput);
  CHECK_THROWS_KIND(Client::connect("no-scheme-here"), ErrorKind::InvalidInput);
}

KEVY_TEST(url_reject_empty_file_path) {
  CHECK_THROWS_KIND(Client::connect("file://"), ErrorKind::InvalidInput);
}

KEVY_TEST(url_is_embedded_flag) {
  Client e = Client::connect(unique_mem_url());
  CHECK(e.is_embedded());
  if (server_available()) {
    auto s = spawn_server();
    Client r = Client::connect(s->url());
    CHECK(!r.is_embedded());
  }
}

KEVY_TEST(url_remote_db_select) {
  if (!server_available()) return;
  auto s = spawn_server();
  // kevy://…/0 does a SELECT 0 round-trip; tcp:// does not. Both must connect.
  Client a = Client::connect("kevy://127.0.0.1:" + std::to_string(s->port) + "/0");
  a.ping();
  Client b = Client::connect("tcp://127.0.0.1:" + std::to_string(s->port));
  b.ping();
}

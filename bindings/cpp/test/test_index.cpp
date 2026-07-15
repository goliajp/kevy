// Declarative indexes IDX.* — remote-only (contract §6 "IDX query").
#include <chrono>
#include <thread>

#include "harness.hpp"
#include "kevy/client.hpp"

using namespace kevy;
using namespace kevy::test;
using namespace std::chrono_literals;

static bool wait_idx_ready(Client& c, const std::string& name) {
  auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(5);
  while (std::chrono::steady_clock::now() < deadline) {
    for (const auto& in : c.idx_list())
      if (in.name == name && in.state == "ready") return true;
    std::this_thread::sleep_for(20ms);
  }
  return false;
}

KEVY_TEST(index_range_paging_and_eq) {
  if (!server_available()) return;
  auto srv = spawn_server();
  Client c = Client::connect(srv->url());

  c.idx_create_range("byage", "user:", "age", IdxType::I64);
  const char* ages[] = {"21", "22", "23", "24", "25"};
  for (int i = 0; i < 5; i++) {
    std::string key = std::string("user:") + char('a' + i);
    c.hset(key, {{"age", ages[i]}});
  }
  CHECK(wait_idx_ready(c, "byage"));

  // idx_list parses IdxInfo including kind.
  bool found = false;
  for (const auto& in : c.idx_list())
    if (in.name == "byage") {
      found = true;
      CHECK_EQ(in.kind, std::string("range"));
    }
  CHECK(found);

  // Range paging: LIMIT 2 pages through 5 rows, cursor ends at null.
  int seen = 0;
  std::optional<std::string> cursor;
  for (int guard = 0; guard < 10; guard++) {
    std::optional<std::string_view> cur;
    if (cursor.has_value()) cur = *cursor;
    IdxPage page = c.idx_query_range("byage", "0", "100", 2, cur);
    seen += static_cast<int>(page.rows.size());
    if (!page.cursor.has_value()) break;
    cursor = page.cursor;
  }
  CHECK_EQ(seen, 5);

  // EQ point lookup.
  IdxPage eq = c.idx_query_eq("byage", "23", 10);
  CHECK_EQ(eq.rows.size(), size_t(1));
  CHECK_EQ(eq.rows[0].value, std::string("23"));

  // drop reports existed, then gone.
  CHECK(c.idx_drop("byage"));
  CHECK(!c.idx_drop("byage"));
}

KEVY_TEST(index_raw_passthrough) {
  if (!server_available()) return;
  auto srv = spawn_server();
  Client c = Client::connect(srv->url());
  // The *_raw escape hatch reaches the full grammar (here: COUNT via command).
  c.idx_create_range("bynum", "n:", "v", IdxType::I64);
  c.hset("n:1", {{"v", "10"}});
  wait_idx_ready(c, "bynum");
  Reply r = c.idx_query_raw({"bynum", "RANGE", "0", "100"});
  CHECK(r.kind == ReplyKind::Array);
}

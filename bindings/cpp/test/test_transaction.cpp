// Transactions — remote-only (contract §6 "Transactions").
#include "harness.hpp"
#include "kevy/client.hpp"
#include "kevy/transaction.hpp"

using namespace kevy;
using namespace kevy::test;

KEVY_TEST(txn_multi_exec_order) {
  if (!server_available()) return;
  auto srv = spawn_server();
  Client c = Client::connect(srv->url());
  Transaction tx = c.multi();
  tx.set("a", "1").incr("n").get("a");
  auto replies = tx.exec();
  CHECK_EQ(replies.size(), size_t(3));
  CHECK(replies[0].kind == ReplyKind::Simple && replies[0].str() == "OK");
  CHECK(replies[1].kind == ReplyKind::Int && replies[1].integer == 1);
  CHECK(replies[2].kind == ReplyKind::Bulk && replies[2].str() == "1");
}

KEVY_TEST(txn_typed_cursor) {
  if (!server_available()) return;
  auto srv = spawn_server();
  Client c = Client::connect(srv->url());
  c.set("k", "v");  // commit before the txn (kevy EXEC has no intra-txn read-your-writes)
  Transaction tx = c.multi();
  tx.set("other", "x").incr("ctr").mget({"k"});
  TransactionReplies cur = tx.exec_typed();
  CHECK_EQ(cur.remaining(), size_t(3));
  cur.next_ok();
  CHECK_EQ(cur.next_int(), int64_t(1));
  auto mg = cur.next_array_of_bulks();
  CHECK_EQ(mg.size(), size_t(1));
  CHECK(mg[0].has_value() && *mg[0] == "v");
  cur.expect_empty();  // arity gate: all consumed
}

KEVY_TEST(txn_watch_abort) {
  if (!server_available()) return;
  auto srv = spawn_server();
  Client c = Client::connect(srv->url());
  Client other = Client::connect(srv->url());
  c.set("wk", "0");
  c.watch({"wk"});
  Transaction tx = c.multi();
  tx.set("wk", "in-txn");
  other.set("wk", "changed");  // concurrent modify by another client
  auto res = tx.exec_watched();
  CHECK(!res.has_value());  // WATCH violation → abort (null)
}

KEVY_TEST(txn_watch_success) {
  if (!server_available()) return;
  auto srv = spawn_server();
  Client c = Client::connect(srv->url());
  c.set("wk", "0");
  c.watch({"wk"});
  Transaction tx = c.multi();
  tx.set("wk", "committed");
  auto res = tx.exec_watched();  // no concurrent modify → commit
  CHECK(res.has_value());
  CHECK_EQ(res->size(), size_t(1));
  auto v = c.get("wk");
  CHECK(v.has_value() && *v == "committed");
}

KEVY_TEST(txn_abandon_implicit_discard) {
  if (!server_available()) return;
  auto srv = spawn_server();
  Client c = Client::connect(srv->url());
  {
    Transaction tx = c.multi();
    tx.set("a", "queued");
    // tx goes out of scope without exec/discard → RAII implicit DISCARD.
  }
  // The socket must not be stuck in MULTI: a normal command still works.
  c.set("ok", "yes");
  auto v = c.get("ok");
  CHECK(v.has_value() && *v == "yes");
  // The abandoned SET must not have applied.
  CHECK(!c.get("a").has_value());
}

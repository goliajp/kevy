// Pipeline — remote-only (contract §6 "Pipeline").
#include "harness.hpp"
#include "kevy/client.hpp"
#include "kevy/pipeline.hpp"

using namespace kevy;
using namespace kevy::test;

KEVY_TEST(pipeline_order_and_inline_errors) {
  if (!server_available()) return;
  auto srv = spawn_server();
  Client c = Client::connect(srv->url());
  auto replies = c.pipeline([](PipelineBuf& p) {
    p.cmd({"SET", "k", "v"});
    p.cmd({"INCR", "k"});   // -ERR (k is not an integer) — must land INLINE
    p.cmd({"GET", "k"});
  });
  CHECK_EQ(replies.size(), size_t(3));
  CHECK(replies[0].kind == ReplyKind::Simple && replies[0].str() == "OK");
  CHECK(replies[1].is_error());  // inline error, batch not aborted
  CHECK(replies[2].kind == ReplyKind::Bulk && replies[2].str() == "v");
}

KEVY_TEST(pipeline_empty_no_wire) {
  if (!server_available()) return;
  auto srv = spawn_server();
  Client c = Client::connect(srv->url());
  auto replies = c.pipeline([](PipelineBuf&) {});  // empty → [] without wire I/O
  CHECK_EQ(replies.size(), size_t(0));
}

KEVY_TEST(pipeline_empty_argv_invalid) {
  if (!server_available()) return;
  auto srv = spawn_server();
  Client c = Client::connect(srv->url());
  CHECK_THROWS_KIND(c.pipeline([](PipelineBuf& p) {
    p.cmd({"PING"});
    p.cmd({});  // empty argv poisons the batch
  }), ErrorKind::InvalidInput);
}

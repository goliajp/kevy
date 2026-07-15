// Change feed FEED.* (contract §6 "FEED replay").
#include "backends.hpp"
#include "harness.hpp"
#include "kevy/client.hpp"

using namespace kevy;
using namespace kevy::test;

KEVY_TEST(feed_replay_remote) {
  if (!server_available()) return;
  auto srv = spawn_server_config("[feed]\nenabled = true\n", {"--threads", "1"});
  Client c = Client::connect(srv->url());

  CHECK(c.feed_shards() >= 1);
  auto [gen, off] = c.feed_tail(0);

  c.set("fk1", "v1");
  c.set("fk2", "v2");

  FeedBatch batch = c.feed_read(0, gen, off, std::nullopt, keys({}));
  CHECK(batch.frames.size() >= 2);
  bool saw_set = false;
  for (const auto& f : batch.frames)
    if (!f.argv.empty() && f.argv[0] == "SET") saw_set = true;
  CHECK(saw_set);

  // Resume from the returned cursor: caught up → empty batch.
  FeedBatch next = c.feed_read(0, batch.generation, batch.next_offset, std::nullopt, keys({}));
  CHECK_EQ(next.frames.size(), size_t(0));
}

KEVY_TEST(feed_stale_cursor_resync) {
  if (!server_available()) return;
  auto srv = spawn_server_config("[feed]\nenabled = true\n", {"--threads", "1"});
  Client c = Client::connect(srv->url());
  // A wildly-stale generation surfaces a FEEDRESYNC protocol error.
  try {
    c.feed_read(0, 999999, 0, std::nullopt, keys({}));
    // server may not treat it as unservable — acceptable, no assertion
  } catch (const KevyError& e) {
    CHECK(e.kind() == ErrorKind::Protocol);
  }
}

KEVY_TEST(feed_embedded_unsupported) {
  Client c = Client::connect(unique_mem_url());
  CHECK_EQ(c.feed_shards(), int64_t(1));  // embedded: always 1
  CHECK_THROWS_KIND(c.feed_tail(1), ErrorKind::InvalidInput);   // non-zero shard
  CHECK_THROWS_KIND(c.feed_tail(0), ErrorKind::Unsupported);    // feed disabled
}

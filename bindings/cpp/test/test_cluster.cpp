// Cluster client CRC16 routing — remote-only (contract §6 "Cluster").
#include "harness.hpp"
#include "kevy/cluster.hpp"

using namespace kevy;
using namespace kevy::test;

KEVY_TEST(cluster_routing) {
  if (!server_available()) return;
  // --cluster with 4 shards: main port P, shard ports P+1..P+4.
  auto srv = spawn_server({"--cluster", "--threads", "4"});
  ClusterClient cc = ClusterClient::connect("127.0.0.1", static_cast<uint16_t>(srv->port));
  CHECK_EQ(cc.shard_count(), size_t(4));

  // Keys spanning slots: each routes to its owner shard with no -MOVED.
  const char* ks[] = {"k0", "k1", "user:42", "rate:10.0.0.1", "gl:abc", "alpha", "beta", "gamma"};
  int i = 0;
  for (const char* k : ks) {
    std::string val = "v" + std::to_string(i++);
    cc.set(k, val);  // throws (Protocol/-MOVED) if routed wrong
    auto got = cc.get(k);
    CHECK(got.has_value() && *got == val);
  }

  cc.incr("counter");
  cc.ping();

  // del/exists route per key and sum across shards.
  CHECK_EQ(cc.del({"k0", "k1", "user:42", "absent"}), int64_t(3));
  CHECK_EQ(cc.exists({"alpha", "beta", "gamma"}), int64_t(3));

  // dbsize is whole-cluster (server fans out internally).
  CHECK(cc.dbsize() >= 1);
  cc.flushall();
}

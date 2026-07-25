// kevy/cluster.hpp — cluster-aware client (contract §3.15). One connection per
// shard, CRC16-slot routed so single-key commands hit their owner shard
// directly (no server -MOVED hop). Topology discovered once at connect via
// CLUSTER SLOTS (16384 slots); CRC16 matches Redis's key_hash_slot exactly.
#ifndef KEVY_CLUSTER_HPP
#define KEVY_CLUSTER_HPP

#include <chrono>
#include <cstdint>
#include <memory>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

#include "kevy/reply.hpp"

namespace kevy {

namespace detail {
class RespConn;
}

using ClusterDuration = std::chrono::milliseconds;

class ClusterClient {
 public:
  // Connect via a seed node, discover topology, open one conn per shard.
  static ClusterClient connect(const std::string& host, uint16_t port);

  ~ClusterClient();
  ClusterClient(ClusterClient&&) noexcept;
  ClusterClient& operator=(ClusterClient&&) noexcept;
  ClusterClient(const ClusterClient&) = delete;
  ClusterClient& operator=(const ClusterClient&) = delete;

  void close();
  size_t shard_count() const { return shards_.size(); }

  Reply request_keyed(std::string_view key, const std::vector<std::string_view>& argv);
  Reply request_unkeyed(const std::vector<std::string_view>& argv);

  void ping();
  int64_t publish(std::string_view channel, std::string_view message);

  void set(std::string_view key, std::string_view value);
  void set_with_ttl(std::string_view key, std::string_view value, ClusterDuration ttl);
  std::optional<std::string> get(std::string_view key);
  int64_t incr(std::string_view key);
  int64_t incr_by(std::string_view key, int64_t delta);
  bool expire(std::string_view key, ClusterDuration ttl);
  bool persist(std::string_view key);
  int64_t ttl_ms(std::string_view key);

  int64_t del(const std::vector<std::string_view>& keys);
  int64_t exists(const std::vector<std::string_view>& keys);
  int64_t dbsize();
  void flushall();

 private:
  ClusterClient() = default;
  detail::RespConn* shard_for(std::string_view key);
  int64_t per_key_sum(const char* verb, const std::vector<std::string_view>& keys);

  std::vector<std::unique_ptr<detail::RespConn>> shards_;
  std::vector<uint16_t> slot_to_shard_;  // length 16384
};

}  // namespace kevy

#endif  // KEVY_CLUSTER_HPP

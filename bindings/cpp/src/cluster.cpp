#include "kevy/cluster.hpp"

#include "argv.hpp"
#include "kevy/crc16.hpp"
#include "kevy/errors.hpp"
#include "reply_util.hpp"
#include "resp_conn.hpp"

namespace kevy {

using detail::i2s;
using detail::raise_reply_error;
using detail::raise_unexpected;

namespace {

constexpr int kNumSlots = 16384;

struct SlotRange {
  uint16_t start, end;
  std::string host;
  uint16_t port;
};

bool as_int(const Reply& r, int64_t& out) {
  if (r.kind == ReplyKind::Int) { out = r.integer; return true; }
  return false;
}
bool as_str(const Reply& r, std::string& out) {
  if (r.kind == ReplyKind::Bulk || r.kind == ReplyKind::Simple) { out = r.bytes; return true; }
  return false;
}
bool in_u16(int64_t n) { return n >= 0 && n <= 65535; }

std::vector<SlotRange> parse_cluster_slots(const Reply& reply) {
  auto bad = []() -> void { throw ProtocolError("malformed CLUSTER SLOTS reply"); };
  if (reply.kind != ReplyKind::Array) bad();
  std::vector<SlotRange> out;
  for (const auto& row : reply.array) {
    if (row.kind != ReplyKind::Array || row.array.size() < 3) bad();
    int64_t start = 0, end = 0, port = 0;
    std::string host;
    const Reply& node = row.array[2];
    if (!as_int(row.array[0], start) || !as_int(row.array[1], end)) bad();
    if (node.kind != ReplyKind::Array || node.array.size() < 2) bad();
    if (!as_str(node.array[0], host) || !as_int(node.array[1], port)) bad();
    if (!in_u16(start) || !in_u16(end) || !in_u16(port)) bad();
    out.push_back(SlotRange{static_cast<uint16_t>(start), static_cast<uint16_t>(end), host,
                            static_cast<uint16_t>(port)});
  }
  return out;
}

// Count expected non-negative; classify error.
int64_t cluster_int(const Reply& r) {
  if (r.kind == ReplyKind::Int) return r.integer;
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}
int64_t cluster_count(const Reply& r) {
  int64_t n = cluster_int(r);
  if (n < 0) throw ProtocolError("expected non-negative count, got " + i2s(n));
  return n;
}
bool cluster_bool(const Reply& r) {
  if (r.kind == ReplyKind::Int) return r.integer == 1;
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}
void cluster_ok(const Reply& r) {
  if (r.kind == ReplyKind::Simple && r.str() == "OK") return;
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}

}  // namespace

ClusterClient ClusterClient::connect(const std::string& host, uint16_t port) {
  auto seed = detail::RespConn::dial(host, port);
  Reply reply = seed->request({"CLUSTER", "SLOTS"});
  seed->close();
  std::vector<SlotRange> ranges = parse_cluster_slots(reply);
  if (ranges.empty()) throw ProtocolError("CLUSTER SLOTS returned no ranges");

  ClusterClient cc;
  cc.slot_to_shard_.assign(kNumSlots, 0);
  std::vector<std::pair<std::string, uint16_t>> nodes;
  for (const auto& r : ranges) {
    int idx = -1;
    for (size_t i = 0; i < nodes.size(); i++)
      if (nodes[i].first == r.host && nodes[i].second == r.port) { idx = static_cast<int>(i); break; }
    if (idx < 0) {
      nodes.emplace_back(r.host, r.port);
      idx = static_cast<int>(nodes.size()) - 1;
    }
    for (int slot = r.start; slot <= r.end; slot++) cc.slot_to_shard_[slot] = static_cast<uint16_t>(idx);
  }
  for (const auto& [h, p] : nodes) cc.shards_.push_back(detail::RespConn::dial(h, p));
  return cc;
}

ClusterClient::~ClusterClient() { close(); }
ClusterClient::ClusterClient(ClusterClient&&) noexcept = default;
ClusterClient& ClusterClient::operator=(ClusterClient&&) noexcept = default;

void ClusterClient::close() { shards_.clear(); }

detail::RespConn* ClusterClient::shard_for(std::string_view key) {
  return shards_[slot_to_shard_[key_hash_slot(key)]].get();
}

Reply ClusterClient::request_keyed(std::string_view key, const std::vector<std::string_view>& argv) {
  return shard_for(key)->request(argv);
}
Reply ClusterClient::request_unkeyed(const std::vector<std::string_view>& argv) {
  return shards_[0]->request(argv);
}

void ClusterClient::ping() {
  Reply r = request_unkeyed({"PING"});
  if (r.kind == ReplyKind::Simple && (r.str() == "PONG" || r.str() == "OK")) return;
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}

int64_t ClusterClient::publish(std::string_view channel, std::string_view message) {
  return cluster_count(request_unkeyed({"PUBLISH", channel, message}));
}

void ClusterClient::set(std::string_view key, std::string_view value) {
  cluster_ok(request_keyed(key, {"SET", key, value}));
}
void ClusterClient::set_with_ttl(std::string_view key, std::string_view value, ClusterDuration ttl) {
  int64_t ms = ttl.count() < 0 ? 0 : ttl.count();
  cluster_ok(request_keyed(key, {"SET", key, value, "PX", i2s(ms)}));
}
std::optional<std::string> ClusterClient::get(std::string_view key) {
  Reply r = request_keyed(key, {"GET", key});
  if (r.kind == ReplyKind::Bulk) return std::move(r.bytes);
  if (r.is_nil()) return std::nullopt;
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}
int64_t ClusterClient::incr(std::string_view key) {
  return cluster_int(request_keyed(key, {"INCR", key}));
}
int64_t ClusterClient::incr_by(std::string_view key, int64_t delta) {
  return cluster_int(request_keyed(key, {"INCRBY", key, i2s(delta)}));
}
bool ClusterClient::expire(std::string_view key, ClusterDuration ttl) {
  int64_t ms = ttl.count() < 0 ? 0 : ttl.count();
  return cluster_bool(request_keyed(key, {"PEXPIRE", key, i2s(ms)}));
}
bool ClusterClient::persist(std::string_view key) {
  return cluster_bool(request_keyed(key, {"PERSIST", key}));
}
int64_t ClusterClient::ttl_ms(std::string_view key) {
  return cluster_int(request_keyed(key, {"PTTL", key}));
}

int64_t ClusterClient::per_key_sum(const char* verb, const std::vector<std::string_view>& keys) {
  int64_t total = 0;
  for (auto k : keys) {
    int64_t n = cluster_int(request_keyed(k, {verb, k}));
    if (n > 0) total += n;
  }
  return total;
}
int64_t ClusterClient::del(const std::vector<std::string_view>& keys) { return per_key_sum("DEL", keys); }
int64_t ClusterClient::exists(const std::vector<std::string_view>& keys) { return per_key_sum("EXISTS", keys); }
int64_t ClusterClient::dbsize() { return cluster_count(request_unkeyed({"DBSIZE"})); }
void ClusterClient::flushall() { cluster_ok(request_unkeyed({"FLUSHALL"})); }

}  // namespace kevy

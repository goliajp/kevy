// kevy/client.hpp — the unified client (contract §1–§4).
//
// One Client, opaque about whether the backend is the in-process embedded
// engine (mem:// / file://) or a remote RESP server (kevy:// / redis:// /
// tcp://). A single connect(url) chooses; the same business code runs against
// either by changing only the URL string (§1.1).
//
// Every command is exposed on BOTH a blocking face (the methods here) and an
// async face (async(), returning std::future — §1.4). They agree because the
// async face delegates to these blocking methods. Errors are thrown as a
// KevyError subclass (§2).
//
// All key/value/member/field parameters are std::string_view — binary-safe,
// never assumed UTF-8 (§7). Nullable returns use std::optional.
#ifndef KEVY_CLIENT_HPP
#define KEVY_CLIENT_HPP

#include <chrono>
#include <cstdint>
#include <functional>
#include <memory>
#include <optional>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include "kevy/embedded.hpp"
#include "kevy/enums.hpp"
#include "kevy/reply.hpp"
#include "kevy/types.hpp"

namespace kevy {

namespace detail {
class RespConn;
}
class Transaction;
class PipelineBuf;
class AsyncClient;

using Bytes = std::string;
using OptBytes = std::optional<std::string>;
using ByteList = std::vector<std::string_view>;
using Duration = std::chrono::milliseconds;

class Client {
 public:
  // Open a backend chosen by URL scheme (§1.1). Rejects TLS/AUTH and unknown
  // schemes before any I/O; for kevy:// / redis:// with a /db, issues one
  // SELECT round-trip.
  static Client connect(const std::string& url);

  ~Client();
  Client(Client&&) noexcept;
  Client& operator=(Client&&) noexcept;
  Client(const Client&) = delete;
  Client& operator=(const Client&) = delete;

  void close();
  bool is_embedded() const { return emb_ != nullptr; }

  // The async face over this same client (§1.4).
  AsyncClient async();

  // Mandatory raw-argv escape hatch (§7): run any verb, get the raw Reply. A
  // -ERR arrives as a Reply with is_error() (data, not a thrown error).
  Reply command(const std::vector<std::string>& argv);

  // --- core string / generic key (§3.1) ---
  void ping();
  void set(std::string_view key, std::string_view value);
  OptBytes get(std::string_view key);
  int64_t del(const ByteList& keys);
  int64_t exists(const ByteList& keys);
  int64_t incr(std::string_view key);
  int64_t incr_by(std::string_view key, int64_t delta);
  bool expire(std::string_view key, Duration ttl);
  bool persist(std::string_view key);
  int64_t ttl_ms(std::string_view key);
  std::string type_of(std::string_view key);
  int64_t dbsize();
  void flushall();
  void set_with_ttl(std::string_view key, std::string_view value, Duration ttl);
  std::vector<OptBytes> mget(const ByteList& keys);
  void mset(const std::vector<std::pair<std::string_view, std::string_view>>& pairs);
  int64_t publish(std::string_view channel, std::string_view message);

  // --- hash (§3.2) ---
  int64_t hset(std::string_view key, const std::vector<std::pair<std::string_view, std::string_view>>& pairs);
  OptBytes hget(std::string_view key, std::string_view field);
  int64_t hdel(std::string_view key, const ByteList& fields);
  int64_t hlen(std::string_view key);
  std::vector<std::string> hgetall(std::string_view key);
  std::vector<std::string> hkeys(std::string_view key);
  std::vector<std::string> hvals(std::string_view key);

  // --- list (§3.3) ---
  int64_t lpush(std::string_view key, const ByteList& values);
  int64_t rpush(std::string_view key, const ByteList& values);
  std::vector<std::string> lpop(std::string_view key, uint64_t count);
  std::vector<std::string> rpop(std::string_view key, uint64_t count);
  int64_t llen(std::string_view key);
  std::vector<std::string> lrange(std::string_view key, int64_t start, int64_t stop);

  // --- set (§3.4) ---
  int64_t sadd(std::string_view key, const ByteList& members);
  int64_t srem(std::string_view key, const ByteList& members);
  std::vector<std::string> smembers(std::string_view key);
  int64_t scard(std::string_view key);
  bool sismember(std::string_view key, std::string_view member);
  std::vector<std::string> sinter(const ByteList& keys);
  std::vector<std::string> sunion(const ByteList& keys);
  std::vector<std::string> sdiff(const ByteList& keys);

  // --- sorted set (§3.5) ---
  int64_t zadd(std::string_view key, const std::vector<ZMember>& members);
  int64_t zrem(std::string_view key, const ByteList& members);
  std::optional<double> zscore(std::string_view key, std::string_view member);
  int64_t zcard(std::string_view key);
  std::vector<std::string> zrange(std::string_view key, int64_t start, int64_t stop);

  // --- sorted-set algebra (§3.6) ---
  int64_t zinterstore(std::string_view dest, const ByteList& keys);
  int64_t zinterstore_with(std::string_view dest, const ByteList& keys,
                           const std::optional<std::vector<double>>& weights, ZAggregate agg);
  int64_t zunionstore(std::string_view dest, const ByteList& keys);
  int64_t zunionstore_with(std::string_view dest, const ByteList& keys,
                           const std::optional<std::vector<double>>& weights, ZAggregate agg);
  int64_t zintercard(const ByteList& keys, std::optional<uint64_t> limit);

  // --- hash-field TTL (§3.7) ---
  std::vector<HExpireCode> hexpire(std::string_view key, const ByteList& fields, Duration ttl, HExpireCond cond);
  std::vector<HExpireCode> hpexpire(std::string_view key, const ByteList& fields, Duration ttl, HExpireCond cond);
  std::vector<HExpireCode> hpersist(std::string_view key, const ByteList& fields);
  std::vector<int64_t> httl(std::string_view key, const ByteList& fields);
  std::vector<int64_t> hpttl(std::string_view key, const ByteList& fields);

  // --- declarative secondary indexes IDX.* (§3.8, remote-only) ---
  void idx_create_range(std::string_view name, std::string_view prefix, std::string_view field, IdxType ty);
  void idx_create_raw(const std::vector<std::string>& args);
  bool idx_drop(std::string_view name);
  std::vector<IdxInfo> idx_list();
  IdxPage idx_query_range(std::string_view name, std::string_view min, std::string_view max,
                          uint64_t limit, std::optional<std::string_view> cursor);
  IdxPage idx_query_eq(std::string_view name, std::string_view value, uint64_t limit);
  std::vector<Ranked> idx_query_match(std::string_view name, std::string_view text, uint64_t limit);
  std::vector<Ranked> idx_query_knn(std::string_view name, const std::vector<float>& vector, uint64_t k);
  Reply idx_query_raw(const std::vector<std::string>& args);

  // --- change feed / CDC FEED.* (§3.10) ---
  int64_t feed_shards();
  std::pair<uint64_t, uint64_t> feed_tail(uint64_t shard);
  FeedBatch feed_read(uint64_t shard, uint64_t generation, uint64_t offset,
                      std::optional<uint64_t> count, const ByteList& prefixes);

  // --- blocking pops (§3.14) ---
  std::optional<KV> blpop(const ByteList& keys, std::optional<Duration> timeout);
  std::optional<KV> brpop(const ByteList& keys, std::optional<Duration> timeout);
  std::optional<ZPopHit> bzpopmin(const ByteList& keys, std::optional<Duration> timeout);

  // --- transactions (§3.12, remote-only) ---
  void watch(const ByteList& keys);
  void unwatch();
  Transaction multi();

  // --- pipeline (§3.13, remote-only) ---
  std::vector<Reply> pipeline(const std::function<void(PipelineBuf&)>& build);

 private:
  friend class AsyncClient;
  friend class Transaction;
  Client() = default;

  Reply exec(const std::vector<std::string>& argv);
  detail::RespConn* require_remote(const char* feature);

  // typed-reply adapters
  int64_t exec_int(const std::vector<std::string>& argv);
  int64_t exec_count(const std::vector<std::string>& argv);
  void exec_ok(const std::vector<std::string>& argv);
  bool exec_bool(const std::vector<std::string>& argv);
  std::vector<std::string> exec_bulks(const std::vector<std::string>& argv);
  OptBytes exec_opt_bulk(const std::vector<std::string>& argv);

  std::unique_ptr<detail::RespConn> remote_;
  std::shared_ptr<EmbeddedStore> emb_;
  std::string url_;
};

}  // namespace kevy

#endif  // KEVY_CLIENT_HPP

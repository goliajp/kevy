// kevy/types.hpp — public data types (contract §4).
//
// All byte fields are std::string (owned, binary-safe — may contain NUL; the
// client never assumes UTF-8, §7). Optional returns use std::optional.
#ifndef KEVY_TYPES_HPP
#define KEVY_TYPES_HPP

#include <cstdint>
#include <optional>
#include <string>
#include <vector>

namespace kevy {

// (score, member) pair for ZADD.
struct ZMember {
  double score;
  std::string member;
};

// A (key, value) blocking-pop hit (BLPOP/BRPOP).
struct KV {
  std::string key;
  std::string value;
};

// A (key, member, score) BZPOPMIN hit (contract §4.9).
struct ZPopHit {
  std::string key;
  std::string member;
  double score;
};

// One IDX.QUERY hit: row key + the indexed value's wire string form (§4.3).
struct IdxRow {
  std::string key;
  std::string value;
};

// One page of IDX.QUERY RANGE/EQ results (§4.4). cursor is empty when the
// scan is complete (wire "0"); otherwise pass it back to resume.
struct IdxPage {
  std::optional<std::string> cursor;
  std::vector<IdxRow> rows;
};

// One IDX.LIST entry (§4.5). Unknown labels are skipped (forward-compatible).
struct IdxInfo {
  std::string name;
  std::string prefix;
  std::string kind;   // "range" | "unique" | "text" | "ann" | "agg"
  std::string state;  // "ready" | "building"
  uint64_t entries = 0;
  uint64_t bytes = 0;
};

// One (key, score/distance) result from MATCH / KNN.
struct Ranked {
  std::string key;
  double score;
};

// One applied effect's argv at a stream offset (§4.6).
struct FeedFrame {
  uint64_t offset = 0;
  std::vector<std::string> argv;
};

// A run of frames plus the cursor to resume from (§4.7). Resume the next
// feed_read from (generation, next_offset).
struct FeedBatch {
  uint64_t generation = 0;
  uint64_t next_offset = 0;
  std::vector<FeedFrame> frames;
};

// One received pub/sub frame — acks and deliveries alike (§4.8). count is the
// total channels + patterns held after the op. For Unsubscribe/Punsubscribe a
// nullopt channel/pattern means "all".
enum class PubsubKind {
  Subscribe,
  Psubscribe,
  Unsubscribe,
  Punsubscribe,
  Message,
  Pmessage,
};

struct PubsubEvent {
  PubsubKind kind;
  std::optional<std::string> channel;
  std::optional<std::string> pattern;
  std::string payload;
  int64_t count = 0;
};

}  // namespace kevy

#endif  // KEVY_TYPES_HPP

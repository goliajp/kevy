#include "argv.hpp"
#include "kevy/client.hpp"
#include "kevy/errors.hpp"
#include "reply_util.hpp"

// Change feed / CDC: FEED.* (contract §3.10). Both backends serve the same
// cursor contract. This port's embedded connect does not enable the change
// feed (the §5.1 C ABI has no feed-enable flag), so embedded feed_tail/read
// answer Unsupported; feed_shards is always 1 on embedded.

namespace kevy {

using detail::i2s;
using detail::raise_reply_error;
using detail::raise_unexpected;
using detail::u2s;

namespace {

FeedFrame parse_feed_frame(const Reply& f) {
  if (f.kind != ReplyKind::Array || f.array.size() != 2)
    throw ProtocolError("FEED.READ: frame shape != [offset, argv]");
  const Reply& off = f.array[0];
  const Reply& argv_raw = f.array[1];
  if (off.kind != ReplyKind::Int || argv_raw.kind != ReplyKind::Array)
    throw ProtocolError("FEED.READ: frame shape != [offset, argv]");
  FeedFrame frame;
  frame.offset = static_cast<uint64_t>(off.integer);
  for (const auto& a : argv_raw.array) {
    if (a.kind == ReplyKind::Bulk || a.kind == ReplyKind::Simple) frame.argv.push_back(a.bytes);
    else raise_unexpected(a);
  }
  return frame;
}

FeedBatch parse_feed_batch(const Reply& r) {
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  if (r.kind != ReplyKind::Array || r.array.size() != 3)
    throw ProtocolError("FEED.READ: expected [gen, next, frames]");
  const Reply& g = r.array[0];
  const Reply& next = r.array[1];
  const Reply& frames = r.array[2];
  if (g.kind != ReplyKind::Int || next.kind != ReplyKind::Int)
    throw ProtocolError("FEED.READ: non-integer cursor");
  if (frames.kind != ReplyKind::Array) throw ProtocolError("FEED.READ: frames not an array");
  FeedBatch batch;
  batch.generation = static_cast<uint64_t>(g.integer);
  batch.next_offset = static_cast<uint64_t>(next.integer);
  for (const auto& f : frames.array) batch.frames.push_back(parse_feed_frame(f));
  return batch;
}

}  // namespace

int64_t Client::feed_shards() {
  if (emb_) return 1;
  return exec_count({"FEED.SHARDS"});
}

std::pair<uint64_t, uint64_t> Client::feed_tail(uint64_t shard) {
  if (emb_) {
    if (shard != 0) throw InvalidInputError("embedded feed is single-shard: shard must be 0");
    throw UnsupportedError("feed disabled: this port's embedded connect does not enable the change feed");
  }
  Reply r = exec({"FEED.TAIL", u2s(shard)});
  if (r.kind == ReplyKind::Array && r.array.size() == 2 && r.array[0].kind == ReplyKind::Int &&
      r.array[1].kind == ReplyKind::Int)
    return {static_cast<uint64_t>(r.array[0].integer), static_cast<uint64_t>(r.array[1].integer)};
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}

FeedBatch Client::feed_read(uint64_t shard, uint64_t generation, uint64_t offset,
                            std::optional<uint64_t> count, const ByteList& prefixes) {
  if (emb_) {
    if (shard != 0) throw InvalidInputError("embedded feed is single-shard: shard must be 0");
    throw UnsupportedError("feed disabled: this port's embedded connect does not enable the change feed");
  }
  std::vector<std::string> argv{"FEED.READ", u2s(shard), u2s(generation), u2s(offset)};
  if (count.has_value() && *count > 0) {
    argv.emplace_back("COUNT");
    argv.push_back(u2s(*count));
  }
  for (auto p : prefixes) {
    argv.emplace_back("PREFIX");
    argv.emplace_back(p);
  }
  return parse_feed_batch(exec(argv));
}

}  // namespace kevy

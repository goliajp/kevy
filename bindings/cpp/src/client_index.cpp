#include <cstring>

#include "argv.hpp"
#include "kevy/client.hpp"
#include "kevy/errors.hpp"
#include "reply_util.hpp"

// Declarative secondary indexes: IDX.* (contract §3.8). Remote-only — the
// embedded backend answers Unsupported. Two-layered: typed shortcuts plus
// *_raw argv passthroughs that keep every server capability reachable.

namespace kevy {

using detail::i2s;
using detail::raise_reply_error;
using detail::raise_unexpected;
using detail::u2s;

namespace {

const char* idx_type_tag(IdxType t) {
  switch (t) {
    case IdxType::F64: return "f64";
    case IdxType::Str: return "str";
    default: return "i64";
  }
}

IdxPage parse_idx_page(const Reply& r) {
  if (r.kind != ReplyKind::Array || r.array.size() != 2)
    throw ProtocolError("IDX.QUERY page: expected [cursor, rows]");
  IdxPage page;
  const Reply& cur = r.array[0];
  if (cur.kind != ReplyKind::Bulk) raise_unexpected(cur);
  if (cur.bytes != "0") page.cursor = cur.bytes;
  const Reply& flat = r.array[1];
  if (flat.kind != ReplyKind::Array) throw ProtocolError("IDX.QUERY page: rows not an array");
  if (flat.array.size() % 2 != 0) throw ProtocolError("IDX.QUERY page: odd row pair");
  for (size_t i = 0; i + 1 < flat.array.size(); i += 2) {
    const Reply& k = flat.array[i];
    const Reply& v = flat.array[i + 1];
    if (k.kind != ReplyKind::Bulk || v.kind != ReplyKind::Bulk)
      throw ProtocolError("IDX.QUERY page: non-bulk row pair");
    page.rows.push_back(IdxRow{k.bytes, v.bytes});
  }
  return page;
}

std::vector<Ranked> parse_ranked(const Reply& r) {
  if (r.kind != ReplyKind::Array) raise_unexpected(r);
  std::vector<Ranked> out;
  out.reserve(r.array.size());
  for (const auto& row : r.array) {
    if (row.kind != ReplyKind::Array || row.array.size() < 2)
      throw ProtocolError("IDX.QUERY ranked row: expected [key, score]");
    if (row.array[0].kind != ReplyKind::Bulk) raise_unexpected(row.array[0]);
    out.push_back(Ranked{row.array[0].bytes, detail::score_of(row.array[1])});
  }
  return out;
}

IdxInfo parse_idx_info(const Reply& entry) {
  if (entry.kind != ReplyKind::Array) raise_unexpected(entry);
  IdxInfo info;
  const auto& cells = entry.array;
  for (size_t i = 0; i + 1 < cells.size(); i += 2) {
    const Reply& label = cells[i];
    const Reply& value = cells[i + 1];
    if (label.kind != ReplyKind::Bulk || value.kind != ReplyKind::Bulk) continue;
    const std::string& l = label.bytes;
    if (l == "name") info.name = value.bytes;
    else if (l == "prefix") info.prefix = value.bytes;
    else if (l == "kind") info.kind = value.bytes;
    else if (l == "state") info.state = value.bytes;
    else if (l == "entries") info.entries = detail::parse_uint_bytes(value.bytes);
    else if (l == "bytes") info.bytes = detail::parse_uint_bytes(value.bytes);
    // else: forward-compatible — skip unknown labels
  }
  return info;
}

}  // namespace

void Client::idx_create_range(std::string_view name, std::string_view prefix, std::string_view field,
                              IdxType ty) {
  idx_create_raw({std::string(name), "ON", "PREFIX", std::string(prefix), "FIELD", std::string(field),
                  "TYPE", idx_type_tag(ty), "KIND", "range"});
}

void Client::idx_create_raw(const std::vector<std::string>& args) {
  require_remote("IDX.CREATE");
  std::vector<std::string> argv;
  argv.reserve(args.size() + 1);
  argv.emplace_back("IDX.CREATE");
  for (const auto& a : args) argv.push_back(a);
  exec_ok(argv);
}

bool Client::idx_drop(std::string_view name) {
  require_remote("IDX.DROP");
  return exec_bool({"IDX.DROP", std::string(name)});
}

std::vector<IdxInfo> Client::idx_list() {
  require_remote("IDX.LIST");
  Reply r = exec({"IDX.LIST"});
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  if (r.kind != ReplyKind::Array) raise_unexpected(r);
  std::vector<IdxInfo> out;
  out.reserve(r.array.size());
  for (const auto& e : r.array) out.push_back(parse_idx_info(e));
  return out;
}

Reply Client::idx_query_raw(const std::vector<std::string>& args) {
  require_remote("IDX.QUERY");
  std::vector<std::string> argv;
  argv.reserve(args.size() + 1);
  argv.emplace_back("IDX.QUERY");
  for (const auto& a : args) argv.push_back(a);
  Reply r = exec(argv);
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  return r;
}

IdxPage Client::idx_query_range(std::string_view name, std::string_view min, std::string_view max,
                                uint64_t limit, std::optional<std::string_view> cursor) {
  std::vector<std::string> args{std::string(name), "RANGE", std::string(min), std::string(max),
                                "LIMIT", u2s(limit)};
  if (cursor.has_value()) {
    args.emplace_back("CURSOR");
    args.emplace_back(*cursor);
  }
  return parse_idx_page(idx_query_raw(args));
}

IdxPage Client::idx_query_eq(std::string_view name, std::string_view value, uint64_t limit) {
  return parse_idx_page(idx_query_raw({std::string(name), "EQ", std::string(value), "LIMIT", u2s(limit)}));
}

std::vector<Ranked> Client::idx_query_match(std::string_view name, std::string_view text, uint64_t limit) {
  return parse_ranked(idx_query_raw({std::string(name), "MATCH", std::string(text), "LIMIT", u2s(limit)}));
}

std::vector<Ranked> Client::idx_query_knn(std::string_view name, const std::vector<float>& vector,
                                          uint64_t k) {
  // Encode the query vector as a little-endian f32 blob (§7).
  std::string blob(vector.size() * 4, '\0');
  for (size_t i = 0; i < vector.size(); i++) {
    uint32_t bits;
    std::memcpy(&bits, &vector[i], 4);
    blob[i * 4 + 0] = static_cast<char>(bits & 0xFF);
    blob[i * 4 + 1] = static_cast<char>((bits >> 8) & 0xFF);
    blob[i * 4 + 2] = static_cast<char>((bits >> 16) & 0xFF);
    blob[i * 4 + 3] = static_cast<char>((bits >> 24) & 0xFF);
  }
  return parse_ranked(idx_query_raw({std::string(name), "KNN", blob, "LIMIT", u2s(k)}));
}

}  // namespace kevy

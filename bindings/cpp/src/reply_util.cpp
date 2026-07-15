#include "reply_util.hpp"

#include <charconv>
#include <string>

namespace kevy {
namespace detail {

StoreErrorKind classify_store_error(const std::string& s, bool& recognized) {
  recognized = true;
  auto starts = [&](const char* p) { return s.rfind(p, 0) == 0; };
  auto has = [&](const char* p) { return s.find(p) != std::string::npos; };
  if (starts("WRONGTYPE")) return StoreErrorKind::WrongType;
  if (starts("OOM")) return StoreErrorKind::OutOfMemory;
  if (has("is not an integer")) return StoreErrorKind::NotInteger;
  if (has("is not a valid float")) return StoreErrorKind::NotFloat;
  if (has("would overflow")) return StoreErrorKind::Overflow;
  if (has("no such key")) return StoreErrorKind::NoSuchKey;
  if (has("index out of range")) return StoreErrorKind::OutOfRange;
  recognized = false;
  return StoreErrorKind::None;
}

void raise_reply_error(const Reply& r) {
  bool recognized = false;
  StoreErrorKind s = classify_store_error(r.bytes, recognized);
  if (recognized) throw StoreError(s);
  throw ProtocolError(r.str());
}

void raise_unexpected(const Reply& r) {
  throw ProtocolError(std::string("unexpected RESP reply variant: ") + r.shape());
}

std::vector<std::string> array_to_bulks(const Reply& r) {
  std::vector<std::string> out;
  out.reserve(r.array.size());
  for (const auto& it : r.array) {
    switch (it.kind) {
      case ReplyKind::Bulk:
      case ReplyKind::Simple:
        out.push_back(it.bytes);
        break;
      case ReplyKind::Nil:
      case ReplyKind::Null:
        out.emplace_back();  // empty string placeholder for a nil member
        break;
      default:
        raise_unexpected(it);
    }
  }
  return out;
}

double parse_float_bytes(const std::string& b) {
  try {
    size_t used = 0;
    double v = std::stod(b, &used);
    if (used != b.size()) throw ProtocolError("bad float reply: " + b);
    return v;
  } catch (const ProtocolError&) {
    throw;
  } catch (...) {
    throw ProtocolError("bad float reply: " + b);
  }
}

uint64_t parse_uint_bytes(const std::string& b) {
  uint64_t n = 0;
  auto res = std::from_chars(b.data(), b.data() + b.size(), n);
  if (res.ec != std::errc() || res.ptr != b.data() + b.size())
    throw ProtocolError("bad integer reply: " + b);
  return n;
}

std::string format_double(double f) {
  char buf[64];
  auto res = std::to_chars(buf, buf + sizeof(buf), f);
  if (res.ec != std::errc()) return std::to_string(f);
  return std::string(buf, res.ptr);
}

double score_of(const Reply& r) {
  switch (r.kind) {
    case ReplyKind::Bulk:
    case ReplyKind::Simple:
      return parse_float_bytes(r.bytes);
    case ReplyKind::Double:
      return r.dbl;
    default:
      raise_unexpected(r);
  }
  return 0;  // unreachable
}

}  // namespace detail
}  // namespace kevy

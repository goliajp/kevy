#include "kevy/resp.hpp"

#include <charconv>
#include <cstring>

#include "kevy/errors.hpp"

namespace kevy {
namespace resp {

void encode_command(std::string& out, const std::vector<std::string>& argv) {
  out.push_back('*');
  out.append(std::to_string(argv.size()));
  out.append("\r\n");
  for (const auto& a : argv) {
    out.push_back('$');
    out.append(std::to_string(a.size()));
    out.append("\r\n");
    out.append(a);
    out.append("\r\n");
  }
}

namespace {

// Index of the CRLF terminating the payload starting at `from`, or -1.
long find_crlf(const uint8_t* buf, size_t len, size_t from) {
  for (size_t i = from; i + 1 < len; i++) {
    if (buf[i] == '\r' && buf[i + 1] == '\n') return static_cast<long>(i);
  }
  return -1;
}

// Cap a header-declared count by the bytes remaining so a hostile length
// header cannot force a huge allocation (fuzz-hardened).
size_t cap_hint(long count, size_t remaining) {
  if (count < 0) return 0;
  return (static_cast<size_t>(count) > remaining) ? remaining : static_cast<size_t>(count);
}

bool parse_i64(const uint8_t* p, size_t n, int64_t& out) {
  const char* s = reinterpret_cast<const char*>(p);
  auto res = std::from_chars(s, s + n, out);
  return res.ec == std::errc() && res.ptr == s + n;
}

bool parse_double(const uint8_t* p, size_t n, double& out) {
  std::string s(reinterpret_cast<const char*>(p), n);
  try {
    size_t used = 0;
    out = std::stod(s, &used);
    return used == s.size();
  } catch (...) {
    return false;
  }
}

size_t line_reply(const uint8_t* buf, size_t len, ReplyKind kind, Reply& out) {
  long eol = find_crlf(buf, len, 1);
  if (eol < 0) return 0;
  out = Reply{};
  out.kind = kind;
  out.bytes.assign(reinterpret_cast<const char*>(buf + 1), static_cast<size_t>(eol) - 1);
  return static_cast<size_t>(eol) + 2;
}

size_t int_reply(const uint8_t* buf, size_t len, Reply& out) {
  long eol = find_crlf(buf, len, 1);
  if (eol < 0) return 0;
  int64_t n;
  if (!parse_i64(buf + 1, static_cast<size_t>(eol) - 1, n)) throw ProtocolError("bad integer reply");
  out = Reply{};
  out.kind = ReplyKind::Int;
  out.integer = n;
  return static_cast<size_t>(eol) + 2;
}

size_t bulk_reply(const uint8_t* buf, size_t len, ReplyKind kind, Reply& out) {
  long hdr = find_crlf(buf, len, 1);
  if (hdr < 0) return 0;
  int64_t n;
  if (!parse_i64(buf + 1, static_cast<size_t>(hdr) - 1, n)) throw ProtocolError("bad bulk length");
  if (n < 0) {
    out = Reply{};
    out.kind = ReplyKind::Nil;
    return static_cast<size_t>(hdr) + 2;
  }
  size_t start = static_cast<size_t>(hdr) + 2;
  size_t end = start + static_cast<size_t>(n);
  if (len < end + 2) return 0;
  out = Reply{};
  out.kind = kind;
  out.bytes.assign(reinterpret_cast<const char*>(buf + start), static_cast<size_t>(n));
  return end + 2;
}

}  // namespace

// Forward declaration for recursion.
static size_t parse_one(const uint8_t* buf, size_t len, Reply& out);

static size_t agg_reply(const uint8_t* buf, size_t len, ReplyKind kind, Reply& out) {
  long hdr = find_crlf(buf, len, 1);
  if (hdr < 0) return 0;
  int64_t count;
  if (!parse_i64(buf + 1, static_cast<size_t>(hdr) - 1, count)) throw ProtocolError("bad aggregate length");
  if (count < 0) {
    if (kind == ReplyKind::Push) throw ProtocolError("push frame cannot be null");
    out = Reply{};
    out.kind = ReplyKind::Nil;
    return static_cast<size_t>(hdr) + 2;
  }
  size_t pos = static_cast<size_t>(hdr) + 2;
  Reply agg;
  agg.kind = kind;
  agg.array.reserve(cap_hint(count, len - pos));
  for (int64_t i = 0; i < count; i++) {
    Reply item;
    size_t used = parse_one(buf + pos, len - pos, item);
    if (used == 0) return 0;
    agg.array.push_back(std::move(item));
    pos += used;
  }
  out = std::move(agg);
  return pos;
}

static size_t map_reply(const uint8_t* buf, size_t len, Reply& out) {
  long hdr = find_crlf(buf, len, 1);
  if (hdr < 0) return 0;
  int64_t count;
  if (!parse_i64(buf + 1, static_cast<size_t>(hdr) - 1, count) || count < 0)
    throw ProtocolError("bad map length");
  size_t pos = static_cast<size_t>(hdr) + 2;
  Reply m;
  m.kind = ReplyKind::Map;
  m.map.reserve(cap_hint(count, (len - pos) / 2));
  for (int64_t i = 0; i < count; i++) {
    Reply k, v;
    size_t ku = parse_one(buf + pos, len - pos, k);
    if (ku == 0) return 0;
    pos += ku;
    size_t vu = parse_one(buf + pos, len - pos, v);
    if (vu == 0) return 0;
    pos += vu;
    m.map.emplace_back(std::move(k), std::move(v));
  }
  out = std::move(m);
  return pos;
}

static size_t double_reply(const uint8_t* buf, size_t len, Reply& out) {
  long eol = find_crlf(buf, len, 1);
  if (eol < 0) return 0;
  double v;
  if (!parse_double(buf + 1, static_cast<size_t>(eol) - 1, v)) throw ProtocolError("bad double");
  out = Reply{};
  out.kind = ReplyKind::Double;
  out.dbl = v;
  return static_cast<size_t>(eol) + 2;
}

static size_t bool_reply(const uint8_t* buf, size_t len, Reply& out) {
  long eol = find_crlf(buf, len, 1);
  if (eol < 0) return 0;
  if (static_cast<size_t>(eol) - 1 != 1) throw ProtocolError("bad boolean payload");
  char c = static_cast<char>(buf[1]);
  if (c != 't' && c != 'f') throw ProtocolError("bad boolean payload");
  out = Reply{};
  out.kind = ReplyKind::Boolean;
  out.boolean = (c == 't');
  return static_cast<size_t>(eol) + 2;
}

static size_t verbatim_reply(const uint8_t* buf, size_t len, Reply& out) {
  long hdr = find_crlf(buf, len, 1);
  if (hdr < 0) return 0;
  int64_t n;
  if (!parse_i64(buf + 1, static_cast<size_t>(hdr) - 1, n)) throw ProtocolError("bad verbatim length");
  if (n < 4) throw ProtocolError("verbatim length < 4 (fmt + ':')");
  size_t start = static_cast<size_t>(hdr) + 2;
  size_t end = start + static_cast<size_t>(n);
  if (len < end + 2) return 0;
  const uint8_t* body = buf + start;
  if (body[3] != ':') throw ProtocolError("verbatim missing fmt:data separator");
  out = Reply{};
  out.kind = ReplyKind::Verbatim;
  out.verbatim_fmt = {static_cast<char>(body[0]), static_cast<char>(body[1]), static_cast<char>(body[2])};
  out.bytes.assign(reinterpret_cast<const char*>(body + 4), static_cast<size_t>(n) - 4);
  return end + 2;
}

static size_t null_reply(const uint8_t* buf, size_t len, Reply& out) {
  if (len < 3) return 0;
  if (buf[0] != '_' || buf[1] != '\r' || buf[2] != '\n') throw ProtocolError("bad null payload");
  out = Reply{};
  out.kind = ReplyKind::Null;
  return 3;
}

static size_t attributed_reply(const uint8_t* buf, size_t len, Reply& out) {
  Reply discard;
  size_t used = map_reply(buf, len, discard);
  if (used == 0) return 0;
  size_t u2 = parse_one(buf + used, len - used, out);
  if (u2 == 0) return 0;
  return used + u2;
}

static size_t parse_one(const uint8_t* buf, size_t len, Reply& out) {
  if (len == 0) return 0;
  switch (buf[0]) {
    case '+': return line_reply(buf, len, ReplyKind::Simple, out);
    case '-': return line_reply(buf, len, ReplyKind::Error, out);
    case ':': return int_reply(buf, len, out);
    case '$': return bulk_reply(buf, len, ReplyKind::Bulk, out);
    case '*': return agg_reply(buf, len, ReplyKind::Array, out);
    case '%': return map_reply(buf, len, out);
    case '~': return agg_reply(buf, len, ReplyKind::Set, out);
    case ',': return double_reply(buf, len, out);
    case '#': return bool_reply(buf, len, out);
    case '=': return verbatim_reply(buf, len, out);
    case '(': return line_reply(buf, len, ReplyKind::BigNumber, out);
    case '_': return null_reply(buf, len, out);
    case '>': return agg_reply(buf, len, ReplyKind::Push, out);
    case '!': return bulk_reply(buf, len, ReplyKind::BlobError, out);
    case '|': return attributed_reply(buf, len, out);
    default: throw ProtocolError("unknown RESP tag");
  }
}

size_t parse_reply(const uint8_t* buf, size_t len, Reply& out) {
  return parse_one(buf, len, out);
}

Reply decode_reply(const uint8_t* buf, size_t len) {
  Reply out;
  size_t used = parse_reply(buf, len, out);
  if (used == 0) throw ProtocolError("truncated RESP reply");
  return out;
}

static void append_len_line(std::string& out, char tag, long n) {
  out.push_back(tag);
  out.append(std::to_string(n));
  out.append("\r\n");
}

void encode_reply(std::string& out, const Reply& r) {
  switch (r.kind) {
    case ReplyKind::Simple: out.push_back('+'); out.append(r.bytes); out.append("\r\n"); break;
    case ReplyKind::Error: out.push_back('-'); out.append(r.bytes); out.append("\r\n"); break;
    case ReplyKind::BigNumber: out.push_back('('); out.append(r.bytes); out.append("\r\n"); break;
    case ReplyKind::Int: out.push_back(':'); out.append(std::to_string(r.integer)); out.append("\r\n"); break;
    case ReplyKind::Bulk:
      append_len_line(out, '$', static_cast<long>(r.bytes.size()));
      out.append(r.bytes); out.append("\r\n");
      break;
    case ReplyKind::BlobError:
      append_len_line(out, '!', static_cast<long>(r.bytes.size()));
      out.append(r.bytes); out.append("\r\n");
      break;
    case ReplyKind::Nil: out.append("$-1\r\n"); break;
    case ReplyKind::Null: out.append("_\r\n"); break;
    case ReplyKind::Array:
    case ReplyKind::Set:
    case ReplyKind::Push: {
      char tag = r.kind == ReplyKind::Set ? '~' : (r.kind == ReplyKind::Push ? '>' : '*');
      append_len_line(out, tag, static_cast<long>(r.array.size()));
      for (const auto& e : r.array) encode_reply(out, e);
      break;
    }
    case ReplyKind::Map:
      append_len_line(out, '%', static_cast<long>(r.map.size()));
      for (const auto& [k, v] : r.map) { encode_reply(out, k); encode_reply(out, v); }
      break;
    case ReplyKind::Double: out.push_back(','); out.append(std::to_string(r.dbl)); out.append("\r\n"); break;
    case ReplyKind::Boolean: out.append(r.boolean ? "#t\r\n" : "#f\r\n"); break;
    case ReplyKind::Verbatim:
      append_len_line(out, '=', static_cast<long>(r.bytes.size() + 4));
      out.append(r.verbatim_fmt.data(), 3); out.push_back(':'); out.append(r.bytes); out.append("\r\n");
      break;
  }
}

}  // namespace resp

const char* Reply::shape() const {
  switch (kind) {
    case ReplyKind::Simple: return "simple-string";
    case ReplyKind::Error: return "error";
    case ReplyKind::Int: return "integer";
    case ReplyKind::Bulk: return "bulk-string";
    case ReplyKind::Nil:
    case ReplyKind::Null: return "nil";
    case ReplyKind::Array: return "array";
    case ReplyKind::Map: return "map";
    case ReplyKind::Set: return "set";
    case ReplyKind::Double: return "double";
    case ReplyKind::Boolean: return "boolean";
    case ReplyKind::Verbatim: return "verbatim-string";
    case ReplyKind::BigNumber: return "big-number";
    case ReplyKind::Push: return "push";
    case ReplyKind::BlobError: return "blob-error";
  }
  return "unknown";
}

const char* to_string(StoreErrorKind s) {
  switch (s) {
    case StoreErrorKind::WrongType: return "WrongType";
    case StoreErrorKind::NotInteger: return "NotInteger";
    case StoreErrorKind::Overflow: return "Overflow";
    case StoreErrorKind::OutOfRange: return "OutOfRange";
    case StoreErrorKind::NoSuchKey: return "NoSuchKey";
    case StoreErrorKind::NotFloat: return "NotFloat";
    case StoreErrorKind::OutOfMemory: return "OutOfMemory";
    case StoreErrorKind::None: return "OK";
  }
  return "OK";
}

const char* to_string(ErrorKind k) {
  switch (k) {
    case ErrorKind::Store: return "Store";
    case ErrorKind::Io: return "Io";
    case ErrorKind::Protocol: return "Protocol";
    case ErrorKind::ReadOnly: return "ReadOnly";
    case ErrorKind::InvalidInput: return "InvalidInput";
    case ErrorKind::NotFound: return "NotFound";
    case ErrorKind::Unsupported: return "Unsupported";
    case ErrorKind::TimedOut: return "TimedOut";
    case ErrorKind::Closed: return "Closed";
  }
  return "unknown";
}

}  // namespace kevy

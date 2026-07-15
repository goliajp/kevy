#include "url.hpp"

#include <charconv>
#include <mutex>
#include <unordered_map>

#include "kevy/errors.hpp"

namespace kevy {
namespace detail {

std::string Target::registry_key() const {
  switch (kind) {
    case TargetKind::MemNamed: return "mem://" + name;
    case TargetKind::File: return "file://" + path;
    default: return "";
  }
}

namespace {

bool parse_u32(const std::string& s, uint32_t& out) {
  auto res = std::from_chars(s.data(), s.data() + s.size(), out);
  return res.ec == std::errc() && res.ptr == s.data() + s.size();
}

Target parse_remote(const std::string& scheme, const std::string& rest, const std::string& url) {
  if (rest.find('@') != std::string::npos)
    throw UnsupportedError("userinfo (user:pass@host) is unsupported — kevy has no AUTH");
  std::string authority = rest;
  std::string path;
  if (size_t slash = rest.find('/'); slash != std::string::npos) {
    authority = rest.substr(0, slash);
    path = rest.substr(slash + 1);
  }
  std::string host = authority;
  uint16_t port = 6379;
  if (size_t colon = authority.rfind(':'); colon != std::string::npos) {
    host = authority.substr(0, colon);
    std::string ps = authority.substr(colon + 1);
    uint32_t p = 0;
    if (!parse_u32(ps, p) || p > 65535) throw InvalidInputError("bad port: " + ps);
    port = static_cast<uint16_t>(p);
  }
  if (host.empty()) throw InvalidInputError("empty host");
  Target t;
  t.kind = TargetKind::Remote;
  t.host = host;
  t.port = port;
  t.url = url;
  // tcp:// is raw: it never carries a db (ignores any /db, does no SELECT).
  if (scheme != "tcp" && !path.empty()) {
    uint32_t n = 0;
    if (!parse_u32(path, n))
      throw InvalidInputError("bad db index: '" + path + "' (expected a non-negative integer)");
    t.db = n;
  }
  return t;
}

}  // namespace

Target parse_connect_url(const std::string& url) {
  size_t sep = url.find("://");
  if (sep == std::string::npos) throw InvalidInputError("URL missing '://'");
  std::string scheme = url.substr(0, sep);
  std::string rest = url.substr(sep + 3);
  if (scheme == "mem") {
    Target t;
    t.url = url;
    if (rest.empty()) {
      t.kind = TargetKind::MemAnon;
    } else {
      t.kind = TargetKind::MemNamed;
      t.name = rest;
    }
    return t;
  }
  if (scheme == "file") {
    if (rest.empty())
      throw InvalidInputError("file:// URL must include a path (e.g. file:///var/lib/myapp)");
    Target t;
    t.kind = TargetKind::File;
    t.path = rest;
    t.url = url;
    return t;
  }
  if (scheme == "kevy" || scheme == "redis" || scheme == "tcp") {
    return parse_remote(scheme, rest, url);
  }
  if (scheme == "rediss" || scheme == "kevys") {
    throw UnsupportedError("TLS schemes (rediss://, kevys://) are unsupported — kevy has no TLS");
  }
  throw InvalidInputError("unknown URL scheme '" + scheme + "://'");
}

// --- process-global embedded registry (§1.3) ----------------------------
//
// A URL-keyed weak map: two connects with the same mem://<name> or
// file://<path> share one store; the entry evicts when the last strong
// (shared_ptr) handle drops.

namespace {
std::mutex g_registry_mu;
std::unordered_map<std::string, std::weak_ptr<EmbeddedStore>> g_registry;

std::shared_ptr<EmbeddedStore> open_embedded(const Target& t) {
  switch (t.kind) {
    case TargetKind::MemAnon:
    case TargetKind::MemNamed:
      return std::make_shared<EmbeddedStore>(EmbeddedStore::open_mem());
    case TargetKind::File:
      return std::make_shared<EmbeddedStore>(EmbeddedStore::open(t.path));
    default:
      throw InvalidInputError("resolve_store called on a non-embedded target");
  }
}
}  // namespace

std::shared_ptr<EmbeddedStore> resolve_store(const Target& t) {
  std::string key = t.registry_key();
  if (key.empty()) return open_embedded(t);  // anonymous mem:// — never shared
  std::lock_guard<std::mutex> lock(g_registry_mu);
  if (auto it = g_registry.find(key); it != g_registry.end()) {
    if (auto strong = it->second.lock()) return strong;
  }
  auto store = open_embedded(t);
  g_registry[key] = store;
  return store;
}

}  // namespace detail
}  // namespace kevy

// url.hpp — connect-URL parsing + the process-global embedded registry
// (contract §1.1–§1.3). Internal to the library.
#ifndef KEVY_INTERNAL_URL_HPP
#define KEVY_INTERNAL_URL_HPP

#include <cstdint>
#include <memory>
#include <optional>
#include <string>

#include "kevy/embedded.hpp"

namespace kevy {
namespace detail {

enum class TargetKind {
  MemAnon,   // mem:// — fresh, never shared
  MemNamed,  // mem://<name> — shared by name
  File,      // file://<path> — shared + persistent
  Remote,    // kevy:// / redis:// / tcp://
};

struct Target {
  TargetKind kind;
  std::string name;   // mem://<name>
  std::string path;   // file://<path>
  std::string host;   // remote host
  uint16_t port = 6379;
  std::optional<uint32_t> db;  // remote /db index (never set for tcp://)
  std::string url;

  // Process-global registry key for shared embedded targets, or "" for the
  // unshared ones.
  std::string registry_key() const;
};

// Resolve a connect URL to a Target, rejecting TLS/AUTH and unknown schemes
// before any I/O (throws KevyError).
Target parse_connect_url(const std::string& url);

// Open (or share) the embedded store for an embedded target. Two connects
// with the same mem://<name> or file://<path> share one store + bus (§1.3);
// the shared handle evicts when the last strong ref drops (weak map).
std::shared_ptr<EmbeddedStore> resolve_store(const Target& t);

}  // namespace detail
}  // namespace kevy

#endif  // KEVY_INTERNAL_URL_HPP

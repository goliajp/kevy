// backends.hpp — run a test body against both the embedded (mem://) and the
// remote (spawned kevy server) backend, per the §6 "both backends" rule.
#ifndef KEVY_TEST_BACKENDS_HPP
#define KEVY_TEST_BACKENDS_HPP

#include <functional>
#include <string>
#include <vector>

#include "harness.hpp"
#include "kevy/client.hpp"

namespace kevy {
namespace test {

// Invoke fn(client, embedded?) on a fresh embedded client, then (if the server
// binary is present) on a fresh remote client against a freshly spawned server.
inline void both_backends([[maybe_unused]] T& t, const std::function<void(Client&, bool)>& fn) {
  {
    Client c = Client::connect(unique_mem_url());
    fn(c, true);
  }
  if (server_available()) {
    auto srv = spawn_server();
    Client c = Client::connect(srv->url());
    fn(c, false);
  }
}

// Convenience: byte-list from string literals.
inline std::vector<std::string_view> keys(std::initializer_list<std::string_view> ks) {
  return std::vector<std::string_view>(ks);
}

}  // namespace test
}  // namespace kevy

#endif  // KEVY_TEST_BACKENDS_HPP

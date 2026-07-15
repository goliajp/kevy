// harness.hpp — a small dependency-free test harness for the kevy C++ client
// conformance suite (contract §6). Real assertions, real pass/fail counts, and
// a real kevy server spawned for the remote-backend tests.
#ifndef KEVY_TEST_HARNESS_HPP
#define KEVY_TEST_HARNESS_HPP

#include <functional>
#include <memory>
#include <string>
#include <vector>

#include "kevy/errors.hpp"

namespace kevy {
namespace test {

// One test's running context: assertion counters + failure log.
struct T {
  std::string name;
  int checks = 0;
  int fails = 0;
  std::vector<std::string> failures;
  void fail(const std::string& msg) {
    fails++;
    failures.push_back(msg);
  }
};

struct Case {
  const char* name;
  std::function<void(T&)> fn;
};

std::vector<Case>& registry();
int register_case(const char* name, std::function<void(T&)> fn);

// A spawned kevy server (remote backend). Killed on destruction.
struct Server {
  int port = 0;
  long pid = 0;
  std::string dir;
  std::string url() const;
  ~Server();
};

// Spawn a fresh single-node server on a free port + temp dir; wait until it
// answers PING. Returns nullptr if the server binary is unavailable. Extra
// args (e.g. --cluster) may be passed.
std::unique_ptr<Server> spawn_server(const std::vector<std::string>& extra = {});

// A unique named mem:// URL (so pub/sub works and stores don't leak).
std::string unique_mem_url();

// Was the server binary found? (Remote tests skip gracefully if not.)
bool server_available();

void expect_throws_kind(T& t, const std::function<void()>& fn, ErrorKind kind, const char* file, int line);

}  // namespace test
}  // namespace kevy

#define KEVY_TEST(nm)                                                        \
  static void nm(kevy::test::T&);                                            \
  static int _kevy_reg_##nm = kevy::test::register_case(#nm, nm);            \
  static void nm([[maybe_unused]] kevy::test::T& t)

#define CHECK(cond)                                                          \
  do {                                                                       \
    t.checks++;                                                              \
    if (!(cond))                                                             \
      t.fail(std::string(__FILE__ ":") + std::to_string(__LINE__) +         \
             " CHECK failed: " #cond);                                       \
  } while (0)

#define CHECK_MSG(cond, msg)                                                 \
  do {                                                                       \
    t.checks++;                                                              \
    if (!(cond))                                                             \
      t.fail(std::string(__FILE__ ":") + std::to_string(__LINE__) + " " +   \
             (msg));                                                         \
  } while (0)

#define CHECK_EQ(a, b)                                                       \
  do {                                                                       \
    t.checks++;                                                              \
    auto _va = (a);                                                          \
    auto _vb = (b);                                                          \
    if (!(_va == _vb))                                                       \
      t.fail(std::string(__FILE__ ":") + std::to_string(__LINE__) +         \
             " CHECK_EQ failed: " #a " != " #b);                            \
  } while (0)

#define CHECK_THROWS_KIND(expr, kind)                                        \
  kevy::test::expect_throws_kind(                                            \
      t, [&]() { (void)(expr); }, kind, __FILE__, __LINE__)

#endif  // KEVY_TEST_HARNESS_HPP

// main.cpp — the harness implementation (registry, server spawn) + the runner.
#include <arpa/inet.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include <atomic>
#include <chrono>
#include <cstdlib>
#include <cstring>
#include <map>
#include <thread>

#include "harness.hpp"
#include "kevy/client.hpp"

namespace kevy {
namespace test {

std::vector<Case>& registry() {
  static std::vector<Case> r;
  return r;
}
int register_case(const char* name, std::function<void(T&)> fn) {
  registry().push_back(Case{name, std::move(fn)});
  return 0;
}

void expect_throws_kind(T& t, const std::function<void()>& fn, ErrorKind kind, const char* file,
                        int line) {
  t.checks++;
  try {
    fn();
    t.fail(std::string(file) + ":" + std::to_string(line) + " expected throw, none");
  } catch (const KevyError& e) {
    if (e.kind() != kind)
      t.fail(std::string(file) + ":" + std::to_string(line) + " wrong error kind: got " +
             to_string(e.kind()));
  } catch (...) {
    t.fail(std::string(file) + ":" + std::to_string(line) + " threw non-KevyError");
  }
}

std::string unique_mem_url() {
  static std::atomic<int> counter{0};
  return "mem://cpptest-" + std::to_string(counter.fetch_add(1));
}

static int free_port() {
  int fd = ::socket(AF_INET, SOCK_STREAM, 0);
  sockaddr_in addr{};
  addr.sin_family = AF_INET;
  addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  addr.sin_port = 0;
  ::bind(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr));
  socklen_t len = sizeof(addr);
  ::getsockname(fd, reinterpret_cast<sockaddr*>(&addr), &len);
  int port = ntohs(addr.sin_port);
  ::close(fd);
  return port;
}

bool server_available() {
#ifdef KEVY_SERVER_BIN
  struct stat st{};
  return ::stat(KEVY_SERVER_BIN, &st) == 0;
#else
  return false;
#endif
}

std::string Server::url() const { return "kevy://127.0.0.1:" + std::to_string(port); }

Server::~Server() {
  if (pid > 0) {
    ::kill(static_cast<pid_t>(pid), SIGKILL);
    int status = 0;
    ::waitpid(static_cast<pid_t>(pid), &status, 0);
  }
}

std::unique_ptr<Server> spawn_server(const std::vector<std::string>& extra) {
  if (!server_available()) return nullptr;
#ifdef KEVY_SERVER_BIN
  int port = free_port();
  char tmpl[] = "/tmp/kevy-cpp-XXXXXX";
  char* dir = ::mkdtemp(tmpl);
  if (dir == nullptr) return nullptr;

  std::vector<std::string> args = {KEVY_SERVER_BIN, "--bind", "127.0.0.1", "--port",
                                   std::to_string(port), "--dir", dir};
  for (const auto& e : extra) args.push_back(e);

  pid_t pid = ::fork();
  if (pid == 0) {
    int devnull = ::open("/dev/null", O_WRONLY);
    if (devnull >= 0) {
      ::dup2(devnull, STDOUT_FILENO);
      ::dup2(devnull, STDERR_FILENO);
    }
    std::vector<char*> cargs;
    for (auto& a : args) cargs.push_back(const_cast<char*>(a.c_str()));
    cargs.push_back(nullptr);
    ::execv(KEVY_SERVER_BIN, cargs.data());
    ::_exit(127);
  }
  if (pid < 0) return nullptr;

  auto srv = std::make_unique<Server>();
  srv->port = port;
  srv->pid = pid;
  srv->dir = dir;

  // Wait until it answers PING (up to ~10s).
  auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(10);
  while (std::chrono::steady_clock::now() < deadline) {
    try {
      Client c = Client::connect(srv->url());
      c.ping();
      return srv;
    } catch (...) {
      std::this_thread::sleep_for(std::chrono::milliseconds(30));
    }
  }
  return srv;  // return anyway; the test will surface the failure
#else
  (void)extra;
  return nullptr;
#endif
}

std::unique_ptr<Server> spawn_server_config(const std::string& toml,
                                            const std::vector<std::string>& extra) {
  if (!server_available()) return nullptr;
  char tmpl[] = "/tmp/kevy-cpp-cfg-XXXXXX";
  char* dir = ::mkdtemp(tmpl);
  if (dir == nullptr) return nullptr;
  std::string cfg = std::string(dir) + "/kevy.toml";
  FILE* f = ::fopen(cfg.c_str(), "w");
  if (f == nullptr) return nullptr;
  ::fwrite(toml.data(), 1, toml.size(), f);
  ::fclose(f);
  std::vector<std::string> args = {"--config", cfg};
  for (const auto& e : extra) args.push_back(e);
  return spawn_server(args);
}

}  // namespace test
}  // namespace kevy

int main() {
  using namespace kevy::test;
  int passed = 0, failed = 0, total_checks = 0;
  std::map<std::string, std::pair<int, int>> by_family;  // family → (pass, fail)

  std::printf("kevy C++ client conformance — %zu tests\n", registry().size());
  std::printf("remote backend: %s\n\n", server_available() ? "server available" : "SKIPPED (no binary)");

  for (auto& c : registry()) {
    T t;
    t.name = c.name;
    try {
      c.fn(t);
    } catch (const std::exception& e) {
      t.fail(std::string("uncaught exception: ") + e.what());
    } catch (...) {
      t.fail("uncaught non-std exception");
    }
    std::string fam(c.name);
    if (auto us = fam.find('_'); us != std::string::npos) fam = fam.substr(0, us);
    bool ok = t.fails == 0;
    total_checks += t.checks;
    if (ok) {
      passed++;
      by_family[fam].first++;
    } else {
      failed++;
      by_family[fam].second++;
      std::printf("FAIL %s (%d checks, %d failed)\n", c.name, t.checks, t.fails);
      for (const auto& f : t.failures) std::printf("      %s\n", f.c_str());
    }
  }

  std::printf("\n--- per family ---\n");
  for (const auto& [fam, pf] : by_family)
    std::printf("  %-12s %d passed, %d failed\n", fam.c_str(), pf.first, pf.second);

  std::printf("\n=== %d passed, %d failed (%d tests, %d assertions) ===\n", passed, failed,
              passed + failed, total_checks);
  return failed == 0 ? 0 : 1;
}

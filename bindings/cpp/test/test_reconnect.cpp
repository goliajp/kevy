// Reconnect / robustness (contract §6 "Reconnect / robustness").
#include <signal.h>

#include "harness.hpp"
#include "kevy/client.hpp"

using namespace kevy;
using namespace kevy::test;

KEVY_TEST(reconnect_closed_after_close) {
  if (!server_available()) return;
  auto srv = spawn_server();
  std::string url = srv->url();
  Client c = Client::connect(url);
  c.set("k", "v");
  c.close();
  // A call on a closed client surfaces Closed.
  CHECK_THROWS_KIND(c.get("k"), ErrorKind::Closed);
  // Reconnect on a fresh connect resumes commands.
  Client c2 = Client::connect(url);
  auto v = c2.get("k");
  CHECK(v.has_value() && *v == "v");
}

KEVY_TEST(reconnect_dropped_mid_session) {
  if (!server_available()) return;
  auto srv = spawn_server();
  std::string url = srv->url();
  Client c = Client::connect(url);
  c.set("k", "v");
  // Kill the server out from under the live connection.
  ::kill(static_cast<pid_t>(srv->pid), SIGKILL);
  srv.reset();  // reap
  // The next request surfaces a KevyError (Closed/Io), never a silent success.
  bool threw = false;
  try {
    for (int i = 0; i < 5; i++) c.get("k");  // provoke the broken pipe/EOF
  } catch (const KevyError& e) {
    threw = true;
    CHECK(e.kind() == ErrorKind::Closed || e.kind() == ErrorKind::Io);
  }
  CHECK(threw);
}

KEVY_TEST(reconnect_connect_refused_is_io) {
  // A refused connection surfaces at connect time as an Io error.
  CHECK_THROWS_KIND(Client::connect("kevy://127.0.0.1:1"), ErrorKind::Io);
}

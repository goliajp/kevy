// Pub/sub round-trip (contract §6 "Pub/sub round-trip").
#include <chrono>
#include <thread>

#include "harness.hpp"
#include "kevy/client.hpp"
#include "kevy/subscriber.hpp"

using namespace kevy;
using namespace kevy::test;
using namespace std::chrono_literals;

KEVY_TEST(pubsub_embedded_named_bus) {
  // Named mem:// bus: a Client publisher and a Subscriber find each other.
  std::string url = unique_mem_url();
  Subscriber sub = Subscriber::connect_channels(url, {"room"});
  Client pub = Client::connect(url);
  // small settle so the subscription is registered on the shared bus
  std::this_thread::sleep_for(20ms);
  pub.publish("room", "hi");
  sub.set_read_timeout(std::optional<SubDuration>(2000ms));
  auto [chan, payload] = sub.recv_message();
  CHECK_EQ(chan, std::string("room"));
  CHECK_EQ(payload, std::string("hi"));
}

KEVY_TEST(pubsub_anon_mem_rejected) {
  CHECK_THROWS_KIND(Subscriber::connect("mem://"), ErrorKind::Unsupported);
}

KEVY_TEST(pubsub_remote_message) {
  if (!server_available()) return;
  auto srv = spawn_server();
  Subscriber sub = Subscriber::connect_channels(srv->url(), {"room"});
  Client pub = Client::connect(srv->url());
  std::this_thread::sleep_for(50ms);
  int64_t n = pub.publish("room", "hello");
  CHECK(n >= 1);
  sub.set_read_timeout(std::optional<SubDuration>(2000ms));
  auto [chan, payload] = sub.recv_message();
  CHECK_EQ(chan, std::string("room"));
  CHECK_EQ(payload, std::string("hello"));
}

KEVY_TEST(pubsub_remote_pmessage) {
  if (!server_available()) return;
  auto srv = spawn_server();
  Subscriber sub = Subscriber::connect(srv->url());
  sub.psubscribe({"ro*"});
  Client pub = Client::connect(srv->url());
  std::this_thread::sleep_for(50ms);
  pub.publish("room", "glob");
  sub.set_read_timeout(std::optional<SubDuration>(2000ms));
  auto [chan, payload] = sub.recv_message();  // Pmessage: channel is concrete
  CHECK_EQ(chan, std::string("room"));
  CHECK_EQ(payload, std::string("glob"));
}

KEVY_TEST(pubsub_remote_hello3_push) {
  if (!server_available()) return;
  auto srv = spawn_server();
  Subscriber sub = Subscriber::connect(srv->url());
  sub.hello3();  // upgrade to RESP3 push frames (must precede subscribe)
  sub.subscribe({"room"});
  Client pub = Client::connect(srv->url());
  std::this_thread::sleep_for(50ms);
  pub.publish("room", "v3");
  sub.set_read_timeout(std::optional<SubDuration>(2000ms));
  auto [chan, payload] = sub.recv_message();  // recv handles RESP3 push transparently
  CHECK_EQ(payload, std::string("v3"));
}

KEVY_TEST(pubsub_read_timeout_bounds_recv) {
  if (!server_available()) return;
  auto srv = spawn_server();
  Subscriber sub = Subscriber::connect_channels(srv->url(), {"quiet"});
  sub.set_read_timeout(std::optional<SubDuration>(100ms));
  // No publisher → recv must time out (TimedOut), not block forever.
  CHECK_THROWS_KIND(sub.recv_message(), ErrorKind::TimedOut);
}

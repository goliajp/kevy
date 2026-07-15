// kevy/async.hpp — the async face (contract §1.4). ONE client exposes both
// faces: the blocking methods on Client, and this AsyncClient reached via
// Client::async(). They AGREE because every async method delegates to the
// same blocking method on a std::async thread and resolves a std::future — so
// the async result is byte-for-byte the sync result.
//
// C++ has blocking sockets, so this works on BOTH backends (embedded + remote).
// Note: a Client is not safe for concurrent use; drive one async call at a
// time (or one Client per outstanding future), exactly as the blocking face.
#ifndef KEVY_ASYNC_HPP
#define KEVY_ASYNC_HPP

#include <future>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include "kevy/client.hpp"

namespace kevy {

class AsyncClient {
 public:
  explicit AsyncClient(Client* c) : c_(c) {}
  Client* client() const { return c_; }

  // Generic escape hatch: run any blocking op on a future.
  template <typename Fn>
  auto run(Fn&& fn) -> std::future<decltype(fn(std::declval<Client&>()))> {
    Client* c = c_;
    return std::async(std::launch::async, [c, fn = std::forward<Fn>(fn)]() { return fn(*c); });
  }

  std::future<Reply> command(std::vector<std::string> argv) {
    Client* c = c_;
    return std::async(std::launch::async, [c, argv = std::move(argv)]() { return c->command(argv); });
  }

  std::future<void> ping() {
    Client* c = c_;
    return std::async(std::launch::async, [c]() { c->ping(); });
  }
  std::future<void> set(std::string key, std::string value) {
    Client* c = c_;
    return std::async(std::launch::async, [c, key = std::move(key), value = std::move(value)]() {
      c->set(key, value);
    });
  }
  std::future<OptBytes> get(std::string key) {
    Client* c = c_;
    return std::async(std::launch::async, [c, key = std::move(key)]() { return c->get(key); });
  }
  std::future<int64_t> del(std::vector<std::string> keys) {
    return list_int(&Client::del, std::move(keys));
  }
  std::future<int64_t> exists(std::vector<std::string> keys) {
    return list_int(&Client::exists, std::move(keys));
  }
  std::future<int64_t> incr(std::string key) {
    Client* c = c_;
    return std::async(std::launch::async, [c, key = std::move(key)]() { return c->incr(key); });
  }
  std::future<int64_t> incr_by(std::string key, int64_t delta) {
    Client* c = c_;
    return std::async(std::launch::async, [c, key = std::move(key), delta]() { return c->incr_by(key, delta); });
  }
  std::future<int64_t> publish(std::string channel, std::string message) {
    Client* c = c_;
    return std::async(std::launch::async, [c, channel = std::move(channel), message = std::move(message)]() {
      return c->publish(channel, message);
    });
  }
  std::future<int64_t> lpush(std::string key, std::vector<std::string> values) {
    return key_list_int(&Client::lpush, std::move(key), std::move(values));
  }
  std::future<int64_t> rpush(std::string key, std::vector<std::string> values) {
    return key_list_int(&Client::rpush, std::move(key), std::move(values));
  }
  std::future<int64_t> sadd(std::string key, std::vector<std::string> members) {
    return key_list_int(&Client::sadd, std::move(key), std::move(members));
  }

 private:
  using ListFn = int64_t (Client::*)(const ByteList&);
  using KeyListFn = int64_t (Client::*)(std::string_view, const ByteList&);

  std::future<int64_t> list_int(ListFn fn, std::vector<std::string> keys) {
    Client* c = c_;
    return std::async(std::launch::async, [c, fn, keys = std::move(keys)]() {
      ByteList bl(keys.begin(), keys.end());
      return (c->*fn)(bl);
    });
  }
  std::future<int64_t> key_list_int(KeyListFn fn, std::string key, std::vector<std::string> vals) {
    Client* c = c_;
    return std::async(std::launch::async, [c, fn, key = std::move(key), vals = std::move(vals)]() {
      ByteList bl(vals.begin(), vals.end());
      return (c->*fn)(key, bl);
    });
  }

  Client* c_;
};

}  // namespace kevy

#endif  // KEVY_ASYNC_HPP

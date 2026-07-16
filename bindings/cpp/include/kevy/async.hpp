// kevy/async.hpp — the async face (contract §1.4). ONE client exposes both
// faces: the blocking methods on Client, and this AsyncClient reached via
// Client::async(). They AGREE because every async method delegates to the
// same blocking method and resolves a std::future — so the async result is
// byte-for-byte the sync result.
//
// A Client is single-threaded (not safe for concurrent use), so the async
// face is backed by ONE reusable worker thread owned by the Client that runs
// submitted ops in FIFO order — not a fresh thread per call. Drive one async
// call at a time (or one Client per outstanding future), exactly as the
// blocking face. Works on BOTH backends (embedded + remote): C++ sockets and
// the embedded engine are blocking, and the worker parks on them.
#ifndef KEVY_ASYNC_HPP
#define KEVY_ASYNC_HPP

#include <condition_variable>
#include <functional>
#include <future>
#include <memory>
#include <mutex>
#include <queue>
#include <string>
#include <string_view>
#include <thread>
#include <utility>
#include <vector>

#include "kevy/client.hpp"

namespace kevy {

namespace detail {

// One reusable worker thread that runs submitted tasks in FIFO order. Created
// lazily by Client::async() and owned by the Client; joined (after draining
// the queue) when the Client closes. submit() packages the op so its result
// or exception lands in the returned future, matching the blocking face.
class AsyncExecutor {
 public:
  AsyncExecutor() : worker_([this] { run(); }) {}
  ~AsyncExecutor() {
    {
      std::lock_guard<std::mutex> lk(m_);
      stop_ = true;
    }
    cv_.notify_all();
    worker_.join();
  }
  AsyncExecutor(const AsyncExecutor&) = delete;
  AsyncExecutor& operator=(const AsyncExecutor&) = delete;

  template <typename R, typename F>
  std::future<R> submit(F&& f) {
    auto task = std::make_shared<std::packaged_task<R()>>(std::forward<F>(f));
    std::future<R> fut = task->get_future();
    {
      std::lock_guard<std::mutex> lk(m_);
      q_.emplace([task] { (*task)(); });
    }
    cv_.notify_one();
    return fut;
  }

 private:
  void run() {
    for (;;) {
      std::function<void()> job;
      {
        std::unique_lock<std::mutex> lk(m_);
        cv_.wait(lk, [this] { return stop_ || !q_.empty(); });
        if (q_.empty()) return;  // stop_ set and drained
        job = std::move(q_.front());
        q_.pop();
      }
      job();
    }
  }

  std::mutex m_;
  std::condition_variable cv_;
  std::queue<std::function<void()>> q_;
  bool stop_ = false;
  std::thread worker_;  // constructed last (after the sync members above)
};

}  // namespace detail

// A thin handle to a Client and its worker; cheap to copy and return by value.
class AsyncClient {
 public:
  AsyncClient(Client* c, detail::AsyncExecutor* exec) : c_(c), exec_(exec) {}
  Client* client() const { return c_; }

  // Generic escape hatch: run any blocking op on the worker.
  template <typename Fn>
  auto run(Fn&& fn) -> std::future<decltype(fn(std::declval<Client&>()))> {
    using R = decltype(fn(std::declval<Client&>()));
    Client* c = c_;
    return exec_->submit<R>([c, fn = std::forward<Fn>(fn)]() { return fn(*c); });
  }

  std::future<Reply> command(std::vector<std::string> argv) {
    Client* c = c_;
    return exec_->submit<Reply>([c, argv = std::move(argv)]() { return c->command(argv); });
  }

  std::future<void> ping() {
    Client* c = c_;
    return exec_->submit<void>([c]() { c->ping(); });
  }
  std::future<void> set(std::string key, std::string value) {
    Client* c = c_;
    return exec_->submit<void>(
        [c, key = std::move(key), value = std::move(value)]() { c->set(key, value); });
  }
  std::future<OptBytes> get(std::string key) {
    Client* c = c_;
    return exec_->submit<OptBytes>([c, key = std::move(key)]() { return c->get(key); });
  }
  std::future<int64_t> del(std::vector<std::string> keys) {
    return list_int(&Client::del, std::move(keys));
  }
  std::future<int64_t> exists(std::vector<std::string> keys) {
    return list_int(&Client::exists, std::move(keys));
  }
  std::future<int64_t> incr(std::string key) {
    Client* c = c_;
    return exec_->submit<int64_t>([c, key = std::move(key)]() { return c->incr(key); });
  }
  std::future<int64_t> incr_by(std::string key, int64_t delta) {
    Client* c = c_;
    return exec_->submit<int64_t>(
        [c, key = std::move(key), delta]() { return c->incr_by(key, delta); });
  }
  std::future<int64_t> publish(std::string channel, std::string message) {
    Client* c = c_;
    return exec_->submit<int64_t>([c, channel = std::move(channel), message = std::move(message)]() {
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
    return exec_->submit<int64_t>([c, fn, keys = std::move(keys)]() {
      ByteList bl(keys.begin(), keys.end());
      return (c->*fn)(bl);
    });
  }
  std::future<int64_t> key_list_int(KeyListFn fn, std::string key, std::vector<std::string> vals) {
    Client* c = c_;
    return exec_->submit<int64_t>([c, fn, key = std::move(key), vals = std::move(vals)]() {
      ByteList bl(vals.begin(), vals.end());
      return (c->*fn)(key, bl);
    });
  }

  Client* c_;
  detail::AsyncExecutor* exec_;
};

}  // namespace kevy

#endif  // KEVY_ASYNC_HPP

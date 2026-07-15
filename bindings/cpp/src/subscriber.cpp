#include "kevy/subscriber.hpp"

#include <algorithm>
#include <chrono>
#include <thread>

#include "argv.hpp"
#include "kevy/embedded.hpp"
#include "kevy/errors.hpp"
#include "kevy/reply.hpp"
#include "reply_util.hpp"
#include "resp_conn.hpp"
#include "url.hpp"

namespace kevy {

using detail::raise_reply_error;

namespace detail {

// Backs an embedded Subscriber with one FFI subscription handle per
// channel/pattern (the C ABI's pub/sub is per-channel; §5.1). Polls the
// handles round-robin; blocks efficiently on the single-handle common case.
struct EmbSub {
  std::shared_ptr<EmbeddedStore> db;  // keeps the shared bus alive
  struct Handle {
    Subscription sub;
    std::string name;
    bool pattern;
  };
  std::vector<Handle> handles;
  size_t rr = 0;

  void add(const std::vector<std::string_view>& names, bool pattern) {
    for (auto n : names) {
      Subscription s = pattern ? db->psubscribe(n) : db->subscribe(n);
      handles.push_back(Handle{std::move(s), std::string(n), pattern});
    }
  }

  void remove(const std::vector<std::string_view>& names, bool pattern) {
    std::vector<Handle> keep;
    for (auto& h : handles) {
      bool drop = h.pattern == pattern &&
                  (names.empty() || std::any_of(names.begin(), names.end(),
                                                [&](std::string_view m) { return m == h.name; }));
      if (!drop) keep.push_back(std::move(h));
      // else: h.sub destructor closes the handle
    }
    handles = std::move(keep);
  }

  // Drain one queued frame from any handle (round-robin), or nullopt.
  std::optional<Reply> poll() {
    size_t n = handles.size();
    for (size_t i = 0; i < n; i++) {
      auto& h = handles[(rr + i) % n];
      if (auto r = h.sub.next()) {
        rr = (rr + i + 1) % n;
        return r;
      }
    }
    return std::nullopt;
  }

  Reply recv(std::optional<SubDuration> timeout) {
    using Clock = std::chrono::steady_clock;
    std::optional<Clock::time_point> end;
    if (timeout.has_value()) end = Clock::now() + *timeout;
    for (;;) {
      if (auto r = poll()) return *r;
      if (handles.size() == 1) {
        uint64_t wait_ms = 0;
        if (timeout.has_value()) {
          auto left = std::chrono::duration_cast<std::chrono::milliseconds>(*end - Clock::now()).count();
          if (left <= 0) throw TimedOutError();
          wait_ms = static_cast<uint64_t>(left);
        }
        if (auto r = handles[0].sub.wait(wait_ms)) return *r;
        if (timeout.has_value()) throw TimedOutError();
        continue;
      }
      if (timeout.has_value() && Clock::now() >= *end) throw TimedOutError();
      std::this_thread::sleep_for(std::chrono::milliseconds(2));
    }
  }
};

}  // namespace detail

namespace {

PubsubEvent ack_event(PubsubKind kind, const std::vector<Reply>& items, bool pattern) {
  if (items.size() != 3) throw ProtocolError("expected 3-element pubsub frame");
  std::optional<std::string> name;
  const Reply& n = items[1];
  if (n.kind == ReplyKind::Bulk) name = n.bytes;
  else if (n.is_nil()) name = std::nullopt;
  else throw ProtocolError("bad pubsub name field");
  if (items[2].kind != ReplyKind::Int) throw ProtocolError("bad pubsub count field");
  PubsubEvent ev;
  ev.kind = kind;
  ev.count = items[2].integer;
  if (pattern) ev.pattern = name;
  else ev.channel = name;
  return ev;
}

PubsubEvent shape_event(const std::string& kind, const std::vector<Reply>& items) {
  if (kind == "subscribe") return ack_event(PubsubKind::Subscribe, items, false);
  if (kind == "psubscribe") return ack_event(PubsubKind::Psubscribe, items, true);
  if (kind == "unsubscribe") return ack_event(PubsubKind::Unsubscribe, items, false);
  if (kind == "punsubscribe") return ack_event(PubsubKind::Punsubscribe, items, true);
  if (kind == "message") {
    if (items.size() != 3 || items[1].kind != ReplyKind::Bulk || items[2].kind != ReplyKind::Bulk)
      throw ProtocolError("bad message frame");
    PubsubEvent ev;
    ev.kind = PubsubKind::Message;
    ev.channel = items[1].bytes;
    ev.payload = items[2].bytes;
    return ev;
  }
  if (kind == "pmessage") {
    if (items.size() != 4 || items[1].kind != ReplyKind::Bulk || items[2].kind != ReplyKind::Bulk ||
        items[3].kind != ReplyKind::Bulk)
      throw ProtocolError("bad pmessage frame");
    PubsubEvent ev;
    ev.kind = PubsubKind::Pmessage;
    ev.pattern = items[1].bytes;
    ev.channel = items[2].bytes;
    ev.payload = items[3].bytes;
    return ev;
  }
  throw ProtocolError("unknown pubsub kind '" + kind + "'");
}

PubsubEvent classify_frame(const Reply& r) {
  const std::vector<Reply>* items = nullptr;
  if (r.kind == ReplyKind::Array || r.kind == ReplyKind::Push) items = &r.array;
  else if (r.kind == ReplyKind::Error) raise_reply_error(r);
  else throw ProtocolError(std::string("expected array/push frame, got ") + r.shape());
  if (items->empty() || (*items)[0].kind != ReplyKind::Bulk)
    throw ProtocolError("pubsub frame missing kind field");
  return shape_event((*items)[0].bytes, *items);
}

}  // namespace

Subscriber::~Subscriber() { close(); }
Subscriber::Subscriber(Subscriber&&) noexcept = default;
Subscriber& Subscriber::operator=(Subscriber&&) noexcept = default;

Subscriber Subscriber::connect(const std::string& url) {
  detail::Target t = detail::parse_connect_url(url);
  Subscriber s;
  switch (t.kind) {
    case detail::TargetKind::MemAnon:
      throw UnsupportedError("anonymous mem:// has no other producer; use mem://<name> for a shared bus");
    case detail::TargetKind::MemNamed:
    case detail::TargetKind::File: {
      s.emb_ = std::make_unique<detail::EmbSub>();
      s.emb_->db = detail::resolve_store(t);
      return s;
    }
    default:
      s.remote_ = detail::RespConn::dial(t.host, t.port);
      return s;
  }
}

Subscriber Subscriber::connect_channels(const std::string& url,
                                        const std::vector<std::string_view>& channels) {
  if (channels.empty())
    throw InvalidInputError("connect_channels needs ≥ 1 channel — use connect for an empty start");
  Subscriber s = connect(url);
  s.subscribe(channels);
  return s;
}

void Subscriber::subscribe(const std::vector<std::string_view>& channels) {
  if (channels.empty()) throw InvalidInputError("SUBSCRIBE needs ≥ 1 channel");
  if (remote_) remote_->write(detail::verb_list("SUBSCRIBE", channels));
  else emb_->add(channels, false);
}

void Subscriber::psubscribe(const std::vector<std::string_view>& patterns) {
  if (patterns.empty()) throw InvalidInputError("PSUBSCRIBE needs ≥ 1 pattern");
  if (remote_) remote_->write(detail::verb_list("PSUBSCRIBE", patterns));
  else emb_->add(patterns, true);
}

void Subscriber::unsubscribe(const std::vector<std::string_view>& channels) {
  if (remote_) remote_->write(detail::verb_list("UNSUBSCRIBE", channels));
  else emb_->remove(channels, false);
}

void Subscriber::punsubscribe(const std::vector<std::string_view>& patterns) {
  if (remote_) remote_->write(detail::verb_list("PUNSUBSCRIBE", patterns));
  else emb_->remove(patterns, true);
}

PubsubEvent Subscriber::hello3() {
  if (!remote_) throw UnsupportedError("HELLO 3 is remote/TCP-only; the embedded bus has no proto switch");
  remote_->write({"HELLO", "3"});
  Reply r = remote_->read_reply();
  if (r.kind == ReplyKind::Map || r.kind == ReplyKind::Array) {
    PubsubEvent ev;
    ev.kind = PubsubKind::Subscribe;
    ev.channel = std::string("HELLO");
    ev.count = 3;
    return ev;
  }
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  throw ProtocolError(std::string("unexpected HELLO 3 reply shape: ") + r.shape());
}

PubsubEvent Subscriber::recv() {
  if (remote_) return classify_frame(remote_->read_reply());
  return classify_frame(emb_->recv(read_timeout_));
}

std::pair<std::string, std::string> Subscriber::recv_message() {
  for (;;) {
    PubsubEvent ev = recv();
    if (ev.kind == PubsubKind::Message || ev.kind == PubsubKind::Pmessage)
      return {ev.channel.value_or(std::string()), ev.payload};
    // else: ack frame — keep waiting
  }
}

void Subscriber::set_read_timeout(std::optional<SubDuration> d) {
  read_timeout_ = d;
  if (remote_) {
    if (d.has_value()) remote_->set_read_timeout(static_cast<uint64_t>(d->count()));
    else remote_->set_read_timeout(std::nullopt);
  }
}

void Subscriber::close() {
  if (remote_) {
    remote_->close();
    remote_.reset();
  }
  emb_.reset();
}

}  // namespace kevy

#include "resp_conn.hpp"

#include <netdb.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

#include <cerrno>
#include <cstring>

#include "kevy/errors.hpp"
#include "kevy/resp.hpp"

namespace kevy {
namespace detail {

std::unique_ptr<RespConn> RespConn::dial(const std::string& host, uint16_t port) {
  struct addrinfo hints{};
  hints.ai_family = AF_UNSPEC;
  hints.ai_socktype = SOCK_STREAM;
  struct addrinfo* res = nullptr;
  std::string port_s = std::to_string(port);
  if (getaddrinfo(host.c_str(), port_s.c_str(), &hints, &res) != 0 || res == nullptr) {
    throw IoError("kevy: getaddrinfo failed for " + host);
  }
  int fd = -1;
  for (struct addrinfo* p = res; p != nullptr; p = p->ai_next) {
    fd = ::socket(p->ai_family, p->ai_socktype, p->ai_protocol);
    if (fd < 0) continue;
    if (::connect(fd, p->ai_addr, p->ai_addrlen) == 0) break;
    ::close(fd);
    fd = -1;
  }
  freeaddrinfo(res);
  if (fd < 0) throw IoError("kevy: connect failed for " + host + ":" + port_s);
  int one = 1;
  ::setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
  return std::unique_ptr<RespConn>(new RespConn(fd));
}

void RespConn::close() {
  if (fd_ >= 0) {
    ::close(fd_);
    fd_ = -1;
  }
}

void RespConn::write_all(const char* data, size_t len) {
  size_t sent = 0;
  while (sent < len) {
    ssize_t n = ::send(fd_, data + sent, len - sent, 0);
    if (n > 0) {
      sent += static_cast<size_t>(n);
      continue;
    }
    if (n < 0 && errno == EINTR) continue;
    close();
    throw IoError(std::string("kevy: send failed: ") + std::strerror(errno));
  }
}

Reply RespConn::read_one() {
  char chunk[8192];
  for (;;) {
    Reply r;
    size_t used = 0;
    try {
      used = resp::parse_reply(reinterpret_cast<const uint8_t*>(rbuf_.data()), rbuf_.size(), r);
    } catch (const KevyError&) {
      throw ProtocolError("malformed reply");
    }
    if (used > 0) {
      rbuf_.erase(0, used);
      return r;
    }
    ssize_t n = ::recv(fd_, chunk, sizeof(chunk), 0);
    if (n > 0) {
      rbuf_.append(chunk, static_cast<size_t>(n));
      continue;
    }
    if (n == 0) {
      close();
      throw ClosedError();  // clean server close mid-read
    }
    if (errno == EINTR) continue;
    if (errno == EAGAIN || errno == EWOULDBLOCK) throw TimedOutError();
    close();
    throw IoError(std::string("kevy: recv failed: ") + std::strerror(errno));
  }
}

Reply RespConn::request(const std::vector<std::string>& argv) {
  if (fd_ < 0) throw ClosedError();
  std::string wire;
  resp::encode_command(wire, argv);
  write_all(wire.data(), wire.size());
  return read_one();
}

std::vector<Reply> RespConn::pipeline_raw(const std::string& wire, size_t n) {
  if (fd_ < 0) throw ClosedError();
  write_all(wire.data(), wire.size());
  std::vector<Reply> out;
  out.reserve(n);
  for (size_t i = 0; i < n; i++) out.push_back(read_one());
  return out;
}

void RespConn::write(const std::vector<std::string>& argv) {
  if (fd_ < 0) throw ClosedError();
  std::string wire;
  resp::encode_command(wire, argv);
  write_all(wire.data(), wire.size());
}

Reply RespConn::read_reply() {
  if (fd_ < 0) throw ClosedError();
  return read_one();
}

void RespConn::set_read_timeout(std::optional<uint64_t> ms) {
  if (fd_ < 0) throw ClosedError();
  struct timeval tv{};
  if (ms.has_value()) {
    tv.tv_sec = static_cast<time_t>(*ms / 1000);
    tv.tv_usec = static_cast<suseconds_t>((*ms % 1000) * 1000);
  }
  ::setsockopt(fd_, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
}

void RespConn::select_db(uint32_t db) {
  Reply r = request({"SELECT", std::to_string(db)});
  if (r.kind == ReplyKind::Error) {
    throw ProtocolError("SELECT " + std::to_string(db) + " rejected: " + r.str());
  }
}

}  // namespace detail
}  // namespace kevy

// resp_conn.hpp — a blocking RESP2/RESP3 TCP connection (contract §1.1), the
// native remote transport written directly on POSIX sockets. One request at a
// time; a read buffer reassembles multi-segment replies. Not thread-safe.
#ifndef KEVY_INTERNAL_RESP_CONN_HPP
#define KEVY_INTERNAL_RESP_CONN_HPP

#include <cstdint>
#include <memory>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

#include "kevy/reply.hpp"

namespace kevy {
namespace detail {

class RespConn {
 public:
  ~RespConn() { close(); }
  RespConn(const RespConn&) = delete;
  RespConn& operator=(const RespConn&) = delete;

  // Dial a TCP RESP connection with TCP_NODELAY (throws IoError on failure).
  static std::unique_ptr<RespConn> dial(const std::string& host, uint16_t port);

  // Send one command and block for exactly one reply.
  Reply request(const std::vector<std::string_view>& argv);
  // Send a pre-encoded batch as one write and read n replies (pipeline).
  std::vector<Reply> pipeline_raw(const std::string& wire, size_t n);
  // Send argv without reading a reply (pub/sub producer side).
  void write(const std::vector<std::string_view>& argv);
  // Block for the next reply frame (pub/sub consumer side).
  Reply read_reply();
  // Bound a blocking read (nullopt clears it). A timeout surfaces as TimedOut.
  void set_read_timeout(std::optional<uint64_t> ms);
  // Issue SELECT n; a -ERR reply throws (fails the connect).
  void select_db(uint32_t db);

  void close();
  bool open() const { return fd_ >= 0; }

 private:
  explicit RespConn(int fd) : fd_(fd) {}
  Reply read_one();
  void write_all(const char* data, size_t len);
  // Drop the already-consumed prefix of rbuf_ when it is fully drained or the
  // dead prefix grows large — keeps reply consumption amortized O(1) instead
  // of an O(n) front-erase per reply (quadratic on a deep pipeline).
  void compact_rbuf();

  int fd_ = -1;
  std::string rbuf_;   // read reassembly buffer; live bytes at [rpos_, size())
  size_t rpos_ = 0;    // read cursor into rbuf_ (consumed prefix length)
  std::string wbuf_;   // reusable request encode buffer (cleared per send)
};

}  // namespace detail
}  // namespace kevy

#endif  // KEVY_INTERNAL_RESP_CONN_HPP

// The C++ smoke: same contract as the C one, through the RAII wrapper.
//
// Build (from the repo root):
//   cargo build -p kevy-ffi
//   c++ -std=c++17 -Icrates/kevy-ffi/include \
//       crates/kevy-ffi/examples/cpp/smoke.cpp \
//       target/debug/libkevy_ffi.a -o /tmp/kevy-smoke-cpp
//   /tmp/kevy-smoke-cpp /tmp/kevy-smoke-cpp-data
#include <cassert>
#include <cstdio>
#include <string>

#include "kevy.hpp"

int main(int argc, char **argv) {
  assert(argc == 2);
  const std::string dir = argv[1];
  std::printf("kevy %.*s\n", static_cast<int>(kevy::Db::version().size()),
              kevy::Db::version().data());

  {
    kevy::Db db(dir);
    assert(db.cmd({"SET", "smoke:cpp", "v1"}).bulk() == "OK");
    assert(db.cmd({"GET", "smoke:cpp"}).bulk() == "v1");
    assert(db.cmd({"NOSUCHVERB"}).is_error());

    auto sub = db.subscribe("c1");
    auto ack = sub.next();
    assert(ack.has_value());  // subscribe ack

    assert(db.cmd({"PUBLISH", "c1", "hello"}).integer() == 1);
    auto frame = sub.next();
    assert(frame.has_value() && frame->arr().size() == 3);
    assert(frame->arr()[0].bulk() == "message");
    assert(frame->arr()[1].bulk() == "c1");
    assert(frame->arr()[2].bulk() == "hello");
    assert(!sub.next().has_value());  // drained
  }  // Db closed here

  {
    kevy::Db db(dir);  // reopen: the key survived
    assert(db.cmd({"GET", "smoke:cpp"}).bulk() == "v1");
    assert(db.cmd({"DEL", "smoke:cpp"}).integer() == 1);
  }

  std::puts("smoke-cpp: ok");
  return 0;
}

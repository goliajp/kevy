#include "kevy/pipeline.hpp"

#include "kevy/client.hpp"
#include "kevy/errors.hpp"
#include "kevy/resp.hpp"
#include "resp_conn.hpp"

namespace kevy {

PipelineBuf& PipelineBuf::cmd(const std::vector<std::string>& parts) {
  if (parts.empty()) {
    poisoned_ = true;
    return *this;
  }
  cmds_.push_back(parts);
  return *this;
}

std::vector<Reply> Client::pipeline(const std::function<void(PipelineBuf&)>& build) {
  detail::RespConn* conn = require_remote("pipeline");
  PipelineBuf p;
  build(p);
  if (p.poisoned_) throw InvalidInputError("pipeline: a queued command had an empty argv");
  if (p.cmds_.empty()) return {};
  std::string wire;
  std::vector<std::string_view> view;
  for (const auto& c : p.cmds_) {
    view.assign(c.begin(), c.end());
    resp::encode_command(wire, view);
  }
  return conn->pipeline_raw(wire, p.cmds_.size());
}

}  // namespace kevy

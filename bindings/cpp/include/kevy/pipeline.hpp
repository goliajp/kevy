// kevy/pipeline.hpp — non-atomic pipeline batching (contract §3.13).
// Remote-only. Queue N commands client-side, send as one write, read N
// replies in order. NOT atomic. Per-command errors come back as Reply::Error
// entries inline (they do not abort the batch). An empty argv poisons the
// batch → InvalidInput at send. An empty batch touches no wire.
#ifndef KEVY_PIPELINE_HPP
#define KEVY_PIPELINE_HPP

#include <cstddef>
#include <string>
#include <vector>

namespace kevy {

class PipelineBuf {
 public:
  // Queue one command (verb + args). Chainable; an empty parts poisons.
  PipelineBuf& cmd(const std::vector<std::string>& parts);
  size_t len() const { return cmds_.size(); }
  bool is_empty() const { return cmds_.empty(); }

 private:
  friend class Client;
  std::vector<std::vector<std::string>> cmds_;
  bool poisoned_ = false;
};

}  // namespace kevy

#endif  // KEVY_PIPELINE_HPP

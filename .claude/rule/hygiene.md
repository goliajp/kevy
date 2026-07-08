# 运行残留与发布门禁纪律(2026-07-08 全面清扫 + v3.17.3 发布事故复盘)

两类教训,都是机械可检的 hard rule。

## 1. 运行残留(residue)

**事故账**:2026-07-08 全面清扫时的存量——kevy 仓库 git 里 16 个
`aof-*.premigration`(v2.7 时代 `git add -A` 收进去的);本地 /tmp
16 个 gate 调试残留目录;lx64 上 home 根 650 个数据文件、9 个旧实验
checkout/快照(合计 >10G)、现役 checkout 内 1993 个未跟踪数据文件。

### 规则

- **不在 repo 根跑 server / embedded store**。数据目录一律 mktemp
  或 bench 脚本的 $DIR。repo 根 .gitignore 已有
  `/aof-*.aof*` `/dump-*.rdb` `/shards.meta` 兜底,但兜底不是许可。
- **`git add -A` 前看 `git status`**(memory
  `feedback-no-git-add-A-in-test-cwd` 的既有铁律,本次再验)。
- **gate / 调试脚本必须 trap 清理**;调试中断(Ctrl-C / kill)留下的
  /tmp 目录在**当轮结束时**顺手 `rm -rf /tmp/kevy-*`,不留给下一轮。
- **远程盒(lx64)每轮 bench 收尾清点**:`ls ~ | grep -E "^(aof-|dump-)"`
  应为 0;/tmp/kevy-* 应为空;旧 checkout 不留(现役 = /root/kevy-dev
  一个)。
- **删除他人/历史目录前先审计**:`git status --porcelain | wc -l` +
  `git log --branches --not --remotes | wc -l`,双 0 才删;非 git
  目录看内容定性(数据残留 vs 快照)再删。

## 2. 发布门禁(release gating)

**事故账**:v3.17.3 首次 tag 时,分支 CI 实为 failure,但
`gh run view <id> -q .conclusion` **只打印 "failure" 而 exit 0**,
`&&` 链照常走完 merge + tag,release workflow 被触发。靠撤 tag +
`gh run cancel` 在 publish 前拦停(crates.io 零污染)。

### 规则

- **CI 结论当门禁时,唯一姿势是 `gh run watch <id> --exit-status`**
  (失败时 exit 非零)。`gh run view -q .conclusion` 是**查询**不是
  **门禁**,它永远 exit 0。
- tag 推送 = publish 触发器,**tag 之前 CI 必须真绿**(不是"应该绿")。
- 误发补救顺序:`git push origin :refs/tags/vX.Y.Z`(撤 tag)→
  `gh run cancel <release-run>`(拦 publish)→ 修根因 → CI 真绿 →
  重新 tag。crates.io publish 不可撤,拦截窗口只在 publish job
  开始前。
- **多步 python 文本 replace 后必须验证命中**(`assert count` 或
  grep 回查)——3.17.3 的 CHANGELOG 段就是 replace anchor 被并行
  改动移位、静默未命中而丢失的。

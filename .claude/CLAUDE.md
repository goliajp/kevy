# kevy — 项目约定

## 沟通语言

**永远用中文沟通。** 所有回复、解释、提问都用中文(代码、标识符、commit message 等技术内容按惯例用英文)。

## 核心约束(已在 L2 锁定,不要擅自更改)

- **纯 Rust + 0 依赖**:`Cargo.toml` 里**不得有任何 crates.io 第三方依赖**,只允许 `std` + 自己的 `kevy-*` crate。
- **不许为算法/数据结构 FFI 找 C**:hashmap、分配、hash、协议解析、reactor 逻辑等全部纯 Rust 自研;**唯一允许的 libc 是完全无法避免的 OS 边界**(socket / poller / mmap / time),且只集中在 `kevy-sys` 里用 `unsafe extern "C"` 手写绑定(不引 `libc` crate)。详见 memory `feedback-pure-rust-no-c-principle`。
- **工具链**:Rust 2024 edition,rust-version 1.97.0。**author = GOLIA K.K.**(workspace 继承)。
- **crate 命名**:一律 `kevy-` 前缀,每个尽量做成可复用的 infra lib。
- **性能目标**:对标并远超 valkey 9.1。基准方法见 `bench/REPORT.md`。

## 代码质量规则(hard rule,新代码必须遵守,旧代码 split 来还债)

- **文件 ≤ 500 LOC**(src/*.rs);test 文件 / `tests/` 目录豁免(Rust 社区惯例)。接近 500 时立即按职责拆 submodule,不要等。
- **函数 ≤ 50 LOC**(包括签名+body+闭合 brace);超了就拆助手函数。例外(需 `// LOC-WAIVER: <理由>` 标注)两类:① 纯数据驱动的 dispatch / match 表(写下来才合理,而非控制流复杂);② **vendored 第三方引擎核心**——从姐妹项目按字节 fork 进来的已测代码(如 spg 的 ERE 匹配器),拆分上游函数会注入 bug 而无可读性收益;waiver 里注明来源。
- **写新 crate 前先想 API surface**:public fn ≤ 7-10 个常用入口;复杂内部用 pub(crate) 隔离。
- **每 commit 后 quick check**: 触动 src/*.rs 的 commit 提交前跑 `wc -l <touched-files>` + grep `^fn \|^pub fn ` 数行;违规当场拆。
- 详细判断标准见 memory `feedback-no-large-file-or-fn`。

## 残留与发布门禁纪律(hard rule)

**[.claude/rule/hygiene.md](rule/hygiene.md)** — 不在 repo 根跑 server;gate 脚本 trap 清理 + 当轮收尾清 /tmp 与远程盒;删历史目录前双零审计(dirty=0 + unpushed=0)。CI 门禁唯一姿势 = `gh run watch --exit-status`(`gh run view -q .conclusion` 是查询不是门禁,永远 exit 0);tag = publish 触发器,tag 前 CI 必须真绿;python replace 后必须验证命中。

## 发布规则(任何 vX.Y.Z 必读)

**[.claude/skills/release/SKILL.md](skills/release/SKILL.md)** — 发布 skill。
版本号活在**七层**(Cargo / 各语言 manifest / 包间互 pin / 活常量 / README
声明 / **vendored 引擎字节** / **Go 模块的主版本**),只改一层就是发了一个谎;
5.1.0 就是这么把 14 个绑定留在 5.0.0 的,其中两个是用字节留的;第七层是
6.0.0 加的 —— Go 把主版本放在导入路径里,写错就永远解析到错的主版本。对齐由 `python3
tools/check_version_alignment.py` **机械判定**(在 CI 里,每层带下限,
查不到东西也算失败),不靠记忆。tag = publish 触发器,**属主扣扳机**。
渠道核验**看内容不看状态码**(软 404 会返回 200)。

## Perf 工作规则(对抗 valkey / redis 必读)

**[.claude/rule/perf-vs-foss.md](rule/perf-vs-foss.md)** — Decomposition + Attack 两步 dance。
任何 perf attack 2 轮 polish 没动针 → 立刻停,切 decomposition 模式。触发词黑名单(`architectural ceiling` / `kernel-bound` / `tied → 停` / `valkey 已 absorbed` / ...)出现一个,本轮思考无效。

## 规划方法

**唯一开放工作清单**:[ROADMAP.md](ROADMAP.md)(线性顺序,从上往下做)。已锁定 OUT-of-scope 见 [.claude/scope-decisions.md](.claude/scope-decisions.md)。当前用户授权 **autorun**(自主推进,完成时报告进度,不必逐步等批准)。

## 常用命令

- 测试:`cargo test --workspace`
- 跑 server:`cargo run -p kevy --bin kevy -- --port 6004`(默认 bind 127.0.0.1;容器内用 `KEVY_BIND=0.0.0.0`)
- 基准对打 valkey:`bash bench/run.sh`
- 开发端口:6004(port-registry 已登记)

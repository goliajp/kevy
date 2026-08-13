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
- **watch 的 `<id>` 必须先锁定再使用**:用
  `gh run list --workflow=ci.yml --branch <b> --limit 1 --json databaseId,headSha`
  取 id,**并核对 headSha == 刚 push 的 commit**。裸
  `gh run list --limit 1` 会抓到别的 workflow 或上一个 commit 的 run,
  watch 它得到的绿是**假绿**(2026-08-01 实证:WINDOW commit 的 CI 红
  被上一 commit 另一 workflow 的绿 run 掩盖了一整轮)。
- **本地门禁读到 verdict 行为止,不是读到 tail 为止。** `locgate` /
  `commentgate` 这类脚本把**判决写在开头或中间**,细节列在后面;
  `... | tail -1` 会把 `FAIL` 那行漏掉,只留下一句像是说明的尾巴。
  **同一个动作在 2026-08-06 连撞两次**:给 `pack()` 加一段注释 →
  只看 tail → 提交 → locgate 其实红了(fn 55 行);修完再犯一次 →
  commentgate 红了(注释里带日期)。**判决行没进眼睛就等于没跑门禁。**
- **修 flake 之前先量它,别凭"这机制看着对"就动手。** 2026-08-06:
  `dispatch_oracle` 在整套 workspace 下三次 accept 超时。第一刀给等待接上
  项目已有的 `KEVY_TEST_PATIENCE` 缩放 —— 机制对、有先例、**却没先确认默认
  倍数**:本地没设那个变量,倍数 1.0,预算还是 10 秒,等于只把提示语写好看了,
  下一跑照红。第二刀才去看服务**怎么起的**:每个 oracle 服务按核数起满分片、
  三个并发起。改成 `--threads 1`(该 oracle 比的是分发语义,对照方本来就是
  单分片)后,同一条命令从"红两次"变成"223 套件 2353 测试 0 失败"。
  **判据必须是同一条命令改前红、改后绿,不是"我用了正确的机制"。**
- **负载相关的 flake,只能用复现那个负载的运行来确认修好了。** 同日续章:
  上面那把 `--threads 1` 之后连绿两次,我据此写下"判据成立" —— 再跑整套
  又红在同一处。两次绿的证据力被高估了:那个超时**只在整机被 200+ 测试
  二进制占满时**出现,而单跑 oracle(1.9 秒全过)根本没造出那个负载。
  真正没解决的是**三个服务同时起**;`--threads 1` 让每个更便宜,没让它们
  不并发。补了一把互斥锁让同一时刻只有一个服务,预算按理由抬到 30 秒。
  **绿的运行如果没复现故障条件,它证明的是"这次没触发",不是"已修好"。**
- **只在 CI 跑的门,看不见本地事故 —— `rootgate` 收尾时必须本地跑一次。**
  2026-08-06:仓库根躺着 **719 个运行时残留**,最早的可追到 **7 月 28 日**,
  也就是这个门红了**九天没人看见**。而它一直在 CI 里、CI 一直是绿的 ——
  因为 **CI 每次都是干净 checkout**:它能抓"测试往 cwd 写"(诱饵实测证明
  测试没有),抓不了"人在仓库根起了个不带 `--dir` 的服务",而后者永远不会
  发生在 CI 上。同日还发现门的形状清单漏了 `tier/` 与 `segs-*`(分层与
  窗口段是这个门写成之后才加的写者),所以连今天那次都没报 —— 门的注释
  自己写着"清单里没有的形状,就是这个门看不见的形状",已补上并双向验证。
- **测试起服务必须给 `--dir <临时目录>`;`rootgate` 现在扫整个工作树,不只仓库根。**
  同日第三次撞到同一族的洞:新写的测试起的服务把 `index-catalog.meta` 写进了
  `crates/kevy-cli/`(测试二进制的 cwd 是**它自己 crate 的目录**,不是仓库根),
  于是**活过了这一轮、在下一轮回答"table already exists"**。三件事同时成立才
  抓到:① 我在测试里断言了 `TABLE.DECLARE` **没有被拒绝**(`request_borrowed`
  的 `unwrap()` 只解 io 错误,`-ERR` 回复会直接溜过去 —— 第一版测试就是这么骗到
  我的);② 顺着 "already exists" 去找文件;③ `rootgate` 的磁盘检查**只看根**。
  已把检查扩到整个工作树(排除 `target/` 与 `.git/`),并双向验证。
  **一小时后同一个错误又犯了一次**:扩检查时清单只写了 `index-catalog.meta`,
  而 sidecar 有**三个**(index / table / view),`table-catalog.meta` 当天下午
  就大摇大摆走过去了。已改成 `*-catalog.meta`。**"清单里没有的形状就是看不见
  的形状"这句话,今天在同一个门上应验了三次** —— 补清单时**去问写者**
  (`grep '\.meta"' crates/*/src/`),不要凭手上这一个例子写。
- tag 推送 = publish 触发器,**tag 之前 CI 必须真绿**(不是"应该绿")。
- 误发补救顺序:`git push origin :refs/tags/vX.Y.Z`(撤 tag)→
  `gh run cancel <release-run>`(拦 publish)→ 修根因 → CI 真绿 →
  重新 tag。crates.io publish 不可撤,拦截窗口只在 publish job
  开始前。
- **多步 python 文本 replace 后必须验证命中**(`assert count` 或
  grep 回查)——3.17.3 的 CHANGELOG 段就是 replace anchor 被并行
  改动移位、静默未命中而丢失的。

## 3. Vendored native 产物纪律(vendored-artifact freshness)

**事故账**:2026-07-16 设备验证发现 expo 门 vendor 的 `libkevy_jni.so`
比 WRONGTYPE 修复早一天 —— 所有 host 套件全绿(它们测的是 fresh build),
但设备上 GET-on-list 返回 phantom miss:**源码级修复根本没进 ship 的
二进制**。同日 vendorgate 首跑又抓到 flutter 门双平台缺整套 shared/raw
符号(真机会 symbol-lookup 崩)+ nitro 四份 header 漂移。

### 规则

- **改了 kevy-ffi / kevy-jni 的 ABI 面(加符号、改 header)后,必须
  重建 + re-vendor 所有门的 native 产物**:
  `packaging/android/build-jnilibs.sh` + `build-ffi-jnilibs.sh` +
  `packaging/apple/build-xcframework.sh` → 各门 `scripts/prepare-native.sh`。
- **门禁 = `bash bench/vendorgate.sh`**(符号清单从源码派生,零维护):
  mobilegate 已把它做硬前置;CI hygiene job 校验 tracked 产物
  (nitro jniLibs、nitro cpp/kevy.h)。release 前必须 PASS。
- host 测试绿 ≠ 设备行为对 —— host 测的是 fresh build,设备跑的是
  vendored binary。on-device smoke 里要有**行为级断言**(如
  GET-on-list → WRONGTYPE),基本-ops smoke 抓不到 stale vendor。
- 共享盒上跑 RN dev app:**Metro 用专用端口**(mobilegate 用 8087),
  默认 8081 会撞别的项目的 Metro,app 会加载**外来 bundle** 并按它
  缺的模块崩(2026-07-16 ExpoLinking 幻影崩溃即此)。

## 判据不许在空数据目录上成立(2026-08-13,downstream 送的)

写下任何"数据活下来了"的判据时,顺手问一句:**拿一个空数据目录跑它,
它还会给出同样的答案吗?** 会的话,这条判据没有在验存储。

来源是 smix 的自我复盘:他们第一版跨版本升级验证拿 `smix sim list`
比对"19 条设备记录相同",而那条命令是去 simctl/adb **枚举设备**、
根本不读 ledger —— 空 store 照样回 19 条。换成真正打印"持久化了什么"
的命令之后,证据才成立。

同一族的失效在 kevy 自己的门里也有,已按此审计并修两处:

- `crashgate` 的 `recovered >= synced`:`synced` 为 0 时恒真,写了零
  条的一轮会把耐久上界报成"达标"。已要求 `synced > 0`,否则 setup 失败。
  (`${marked:-1}` 那个默认值本来就选对了——缺失时比较失败而非通过。)
- `upgrade-interop` 场景 D 的 `OLD_DBSIZE = NEW_DBSIZE`:`0 = 0` 恒真。
  已先断言种子库非空。

**"必须不存在"型断言天生空判据**(空 store 里什么都不存在),永远要和
一条"必须存在"的内容断言配对使用——单独用它等于没验。

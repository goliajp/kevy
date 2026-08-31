# 从 6.2.0 升级到 6.2.1

一句话版本：**服务器什么都没变。** 6.2.1 之所以存在，是因为两个客户端 crate——`kevy-client` 与 `kevy-client-async`——在 crates.io 上停在 2.2.0、基于 5.x 引擎构建，而自 6.0.0 起的每一个 kevy 发布都把它们描述成 6.x。如果你跑的是服务器、用的是某个语言绑定、或者从 Rust 嵌入引擎，无事可做。如果你从 crates.io 依赖这两个客户端 crate 之一，改一行。

## 这个版本为什么存在

- **两个 crate 发了三次却从未送达。** `kevy-client` 与 `kevy-client-async` 走的是自己的版本线（2.x）。工作区升到 6.0.0 时，它们在树内的 manifest 把每个兄弟 crate 都钉到了 6.x，自己的版本号却停在 2.2.0——它们的 API 一点没变。`cargo publish` 拒绝已存在的版本，而发布工作流把这个拒绝当成了"已经发过"。于是 6.0.0、6.1.0、6.2.0 各发出 40 个 crate、跳过这两个，`cargo add kevy-client` 一直解析到一个拉取 `kevy-embedded 5.4.1` 的客户端。是用户发现的，门禁没有。

- **现在每个 crate 都带工作区版本。** 这两个客户端，加上另外三个手写工作区版本号而不是继承它的 crate，都改成了 `version.workspace = true`。一次 bump 移动全部，版本门禁拒绝任何自带版本声明的工作区成员。发布循环也不再听信"已存在"：它把 crates.io 上持有的 manifest 与树将要上传的那份逐项比对，不同即让发布失败。

## 什么原样带过去

- **线协议。** RESP2 与 RESP3 的回复与 6.2.0 逐字节相同。无论是通用 Redis 客户端还是 kevy 自己的客户端，面对服务器都无需任何动作。
- **数据目录。** 6.2.1 就是 6.2.0 的代码换了个号；AOF、快照、value log 与每一种 checkpoint 都跟以前一样打开，两个方向都行。
- **其余所有 crate 与绑定。** 它们从 6.2.0 到 6.2.1 没有任何代码改动。升号是为了让一个数字在所有产物上指同一个发布。
- **客户端 API。** `kevy-client 6.2.1` 就是 `kevy-client 2.2.0` 的 API，只是依赖指向了本次 changelog 描述的引擎。没有任何调用要改。

## 如果你依赖 kevy-client 或 kevy-client-async

把依赖声明改成引擎的主版本：

```toml
[dependencies]
kevy-client = "6"                                              # 原来是 "2"
kevy-client-async = { version = "6", features = ["tokio"] }   # 原来是 "2"
```

写成 `"2"` 的声明会继续解析到 2.2.0，它钉的是 `kevy-embedded ^5.0`，于是把一个 5.4.1 引擎编进你的二进制。如果你的 crate 同时直接依赖 `kevy-resp 6` 或 `kevy-embedded 6`，就会得到每个各两份，并在接缝处报类型错误——这正是那份引出本次发布的报告到达我们手上的原因。

## 推荐步骤

改完声明，让 cargo 证明依赖图里只有一个引擎：

```sh
cargo update -p kevy-client -p kevy-client-async
cargo tree -i kevy-embedded      # 恰好一行，版本 6.2.1
```

如果第二条命令打印出两个版本，说明依赖图里还有别的东西在要 5.x 引擎；`cargo tree -i kevy-embedded@5.4.1` 会点出它的名字。

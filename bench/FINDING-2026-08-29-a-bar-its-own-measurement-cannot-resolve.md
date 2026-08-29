# FINDING 2026-08-29 — a bar the measurement cannot resolve

**Status**: not a regression. The gate now retakes a tripped cell
before calling it a failure, and the contamination it was already
measuring is now acted on.

## What happened

`tailgate`'s `firehose-epoll` cell tripped its 100 ms reactor-gap bar
during the prerelease tier on the merged 6.0.0 tip, at 110 ms with 58%
foreign CPU during probing. Two earlier standalone runs had given 169
ms (107% foreign) and 45 ms (quiet).

The obvious reading was contention, and the obvious reading is the one
this repo's perf methodology names as a trigger word: *"调度方差 /
scheduling variance"* — the answer to which is not to explain the
number away but to make the measurement able to reject it.

## The gate was measuring the contaminant and discarding the answer

`one_run` computes foreign CPU during each probe — everything that is
not this gate's server, its benchmark, or a kernel worker — and prints
it. The comment above that line says the figure exists to tell "I am
busy" from "someone else is", which is the one thing the number can
answer. It was printed and then dropped, and the number that probe
produced was reported as the cell's verdict.

The start check already refuses a box that is busy *before* a run
(idle must be ≥ 80%). Load that arrives *during* one is the same
contamination. So a probe above 25% foreign CPU is discarded and
retaken now, and nine attempts that cannot produce three clean probes
make the gate REFUSE — exit 2, the posture it already takes for a
tmpfs work dir. A refusal says "this run cannot answer the question".
A FAIL would say "the engine is slow", which the data did not support.

## And then the filter disproved the hypothesis

The next run was 99% idle at start, **no probe was discarded**, and
`firehose-epoll` still came in at 103 ms.

So the contention reading was wrong for that run — and what showed it
was the instrument built to rule contention out.

## The A/B, which answered the actual question

Same box, same session, alternating binaries: the merged tip and its
pre-merge predecessor (9f30bed0), two rounds each.

| round | pre-merge | post-merge |
|---|---:|---:|
| 1 | 41.9 ms | 78.1 ms |
| 2 | **135.9 ms** | 78.1 ms |

The **pre-merge** binary is the one that blew the bar, at 135.9 ms.
The post-merge binary answered 78.1 ms twice. Whatever this cell is
doing, the fourteen verbs did not cause it.

What the A/B measured instead is the cell's own variation: **3.2× on
identical code**, 41.9 ms to 135.9 ms, against a bar 100 ms away. The
gate's own note beside cell D said as much before this was measured —
"the firehose cell's MEDIAN moves by nearly 2x between rounds, so a
single reading near the bar means little and a trip means something:
read a failure as a real signal before assuming flake" — and then the
gate failed on one reading.

## What changed

A cell that trips is **retaken**, a full sample, and only a trip that
repeats is a verdict. That is the note turned into behaviour: "re-run
it and see" was being left to whoever happened to be watching, which is
how a gate acquires a reputation for flaking instead of a mechanism for
not.

Both thresholds are overridable — `TAILGATE_FOREIGN_MAX`,
`TAILGATE_MAX_ATTEMPTS` — for a box that is genuinely dedicated.

## What is still open, and is the owner's

The bar itself. Cell D's spread on this box means a 100 ms bar sits
inside the measurement's own noise, and two things could fix that which
this file does not do: more samples per round (`RUNS=3` today), or a
bar set where the measurement can resolve it. Which one is a judgement
about what the bar is *for*, and the gate's header says cell B is the
V3 train's named target — so the number it should hold is a decision
about that train, not about this run.

---

# 附:同一天的第二个装置失效 —— query_buffer_limit

`a_streaming_giant_frame_is_disconnected_at_the_cap` 在 release-profile
CI 上红了。归因先说清楚:那一轮唯一的改动是 `bench/tailgate.sh` 和一份
markdown,**两者都不进服务器的编译产物**,而前三轮该 job 全绿。

它有前科 —— 测试自己的注释写着这是它在 CI 里失败过两次的原因,并因此被
重写成 30 秒预算。这是第三次。

## 预算从来不是问题

`free_port()` 探完端口就把 listener 关掉再返回,而 kevy 用 `SO_REUSEPORT`
绑。所以另一台服务器可以占同一个端口,内核把新连接分给其中一台。这时:

- `assert_listening` **成功** —— 确实有东西在监听
- 客户端的字节流到了一台**没有** `KEVY_DEBUG_INPUT_LIMIT=4096` 的服务器
- 那台不会关连接,因为没人要求它关
- 测试打印「the connection was STILL open 30s later」

**这句话和一个真回归会打印的完全一样。** 装置的失效长得跟它自己的数据一样,
所以放宽预算只是让同一个误判来得更晚。

## 现在它先自证

写一个只有本测试写过的 marker,再用六条**新**连接去读。端口若被共用,内核
至少会把一条落到另一台上,断言当场点名「the port is shared with another
server」而不是把它说成引擎的问题。

## 副产物:`CONFIG GET dir` 会报一个没在用的目录

第一版 witness 用的是 `CONFIG GET dir`,因为测试知道自己给的数据目录。它
报回 `.`。

`Runtime::builder().with_data_dir(d)` 设的是**运行时**的数据目录;
`CONFIG GET dir` 读的是 `cfg.server.data_dir`,程序化构建时它停在默认值。
两者不是同一个字段,所以嵌入者问 `CONFIG GET dir` 会被告知一个服务器并没
在写的目录。

发布的 `kevy` 二进制走 Config,两者一致,所以这不影响本次发布 —— 记在这里
是因为它是同一类:**一个面报出的东西,不是另一个面在做的事。**

# kevy-cli 的关系命令

redis-cli 能做的事，kevy-cli 都能做。除此之外，它还带了一组工具，专门处理 kevy
的关系型那一面——[表](tables.md)、[索引](indexes.md)和[视图](views.md)：把目录读成行、
逐页跑查询、导出和恢复声明、CSV 导入导出。

连接选项之后的第一个词，如果恰好是某个工具的小写名字，就运行这个工具；其他词一律当作
服务端命令，和 redis-cli 一样：

```sh
kevy-cli -p 6004 tables            # 工具
kevy-cli -p 6004 TABLE.LIST        # 命令，按 redis-cli 的格式输出
```

工具用的是同一套连接选项（`-h`、`-p`、`-s`、`-u`、`-a`、`--user`、`-n`、`-2`/`-3`），
认证和选库的方式跟发命令完全一样。

## 读目录

`tables`、`indexes`、`views` 把三个目录列成行，都可以带一个名字通配；`indexes`
还可以直接给表名。`describe` 接受表、索引或视图：

```sh
kevy-cli -p 6004 tables 'user*'
kevy-cli -p 6004 indexes users
kevy-cli -p 6004 describe users      # 列、列的类型、读这一列的访问路径
kevy-cli -p 6004 describe+ users     # 同上，再跑一次 TABLE.VERIFY
```

对表，`describe` 显示前缀和主键，每一列的声明类型和读它的编译路径，以及每条访问路径的
构建状态。对索引，显示字段、存储值和编译出它的表；对视图，显示组合树和排序。列信息来自
`TABLE.DESCRIBE`，6.5 之前的服务端没有这个命令。

## 查询

`query` 跑一条 `IDX.QUERY` 或 `VIEW.QUERY` 并输出结果行。`--all` 会顺着游标一页页取，
到 `--max-rows`（默认 10,000）时在整页边界停下，并告诉你从哪个游标接着取：

```sh
kevy-cli -p 6004 query --all IDX.QUERY users.age RANGE 18 65 FIELDS name
kevy-cli -p 6004 explain users.age RANGE 18 65
kevy-cli -p 6004 explain --analyze IDX.QUERY users.age RANGE 18 65
kevy-cli -p 6004 advise
kevy-cli -p 6004 sql run "SELECT id, name FROM users WHERE age BETWEEN 18 AND 65"
```

`explain --analyze` 会真的跑一遍查询，报告的是这个客户端量到的数——往返次数、页数、行数、
耗时——不是服务端的执行剖析。

`sql run` 接受一条 `SELECT`，在客户端按已声明的表做规划，发出能回答它的 `IDX.QUERY`，
只输出 SELECT 列出的列。服务端始终看不到 SQL。没有任何已声明路径能回答的查询会被拒绝，
拒绝文本和 `kevy-cli sql plan` 给的一样，会说明该声明哪个索引——这里没有哪个工具会靠扫描
去补一个 `WHERE`。

## 把声明存成文件

```sh
kevy-cli -p 6004 show-create users                # 能重建它的 TABLE.DECLARE
kevy-cli -p 6004 show-create users --as sql       # CREATE TABLE / CREATE INDEX
kevy-cli -p 6004 dump --schema > schema.kevy      # 先表，再索引，最后视图
kevy-cli -p 6005 run -f schema.kevy               # 在另一台服务端上重放
kevy-cli -p 6004 dump --all ./dump                # schema，加上每张表的行（CSV）
kevy-cli -p 6005 restore ./dump                   # 行、声明、wait-ready、doctor
```

kevy 形式是一行一条命令，加了引号，`run -f` 读回来是同样的词。SQL 形式是
`kevy-cli sql compile` 会编译回同一份声明的 SQL；SQL 表达不了的部分——不是 `<table>:`
的键前缀、`WINDOW`、`AUTODECLARE`——会留成 `-- not carried by SQL` 注释，不会悄悄丢掉。
由表编译出来的索引没有自己的声明：`show-create users.age` 会告诉你是哪张表。

`dump --all` 只写入新目录或空目录：`schema.kevy`、`tables`，以及每张表一个
`table-N.csv`。导出的只有表前缀下的行、而且只有已声明的列；要逐字节复制整个键空间，用
`kevy-cli export`。`restore` 先导入行、再做声明，这样每个索引只从行构建一次，而不是每写
一行更新一次。`restore --from … --to …` 仍然是离线的备份恢复。

## CSV 导入导出

```sh
kevy-cli -p 6004 import-csv users.csv --table users --header
kevy-cli -p 6004 import-csv users.csv --prefix user: --pk id --columns id,name,age
kevy-cli -p 6004 export-csv --table users users.csv
kevy-cli -p 6004 export-csv --table users --via "IDX.QUERY users.age RANGE 18 65" -
```

`import-csv` 每条记录写一个 hash，512 条一批走 pipeline，进度记在 `<file>.progress`，
`--resume` 从那里接着导。空单元格不写字段：缺失的字段就是 NULL。带 `--table` 时，前缀、
主键和列都取自表的声明，并且只写已声明的列；`--key-column` 则直接从某一列取完整的键，
也就是 `export-csv` 写出来的那一列。

`export-csv` 写 RFC 4180 格式的 CSV，键在第一列。不带 `--via` 时用 `SCAN` 找键，会走遍
整个键空间；带 `--via` 时按查询分页取。

## 脚本、等待和轮询

```sh
kevy-cli -p 6004 run -f migrate.kevy --atomic
kevy-cli -p 6004 wait-ready --table users --timeout 60
kevy-cli -p 6004 watch 2 IDX.LIST
kevy-cli -p 6004 status
kevy-cli -p 6004 feed follow --prefix user: --from tail --checkpoint feed.pos
```

`run` 逐行执行 `-f` 文件和 `-c` 命令：全部成功退出 0，用法错误退出 1，连接断开退出 2，
遇到错误回复而停下退出 3（`--force` 会继续跑）。`--atomic` 把脚本包进 `MULTI`/`EXEC`，
并且会明说：失败的命令不会撤销其他命令。`wait-ready` 轮询 `IDX.LIST`，直到指定的索引
构建完成。

## 在 REPL 里

以反斜杠开头的行由 kevy-cli 自己处理，不会发给服务端：

| 行 | 作用 |
|---|---|
| `\dt [pattern]` | `tables` |
| `\di [table\|pattern]` | `indexes` |
| `\dv [pattern]` | `views` |
| `\d name` / `\d+ name` | `describe` / `describe+` |
| `\query …` / `\explain …` / `\advise` | `query` / `explain` / `advise` |
| `\watch seconds command…` | `watch` |
| `\i file` | `run -f file` |
| `\conninfo` | `status` |
| `\x` / `\timing` | 开关展开显示 / 开关每条命令后的耗时 |
| `\?` | 显示这张表 |

连的是 kevy 服务端时，Tab 会在接受对象名的命令后补全表、索引和视图名；在某张表的路径上的
`IDX.QUERY` 里，`FIELDS`、`FILTER`、`SORT`、`DISTINCT`、`FACET` 之后补全这张表的列名。

## 输出格式

```sh
kevy-cli -p 6004 tables --format csv
kevy-cli -p 6004 --json tables
kevy-cli -p 6004 query IDX.QUERY users.age RANGE 18 65 --null NULL --expanded
```

输出到终端时，行按对齐的表格显示；接管道时，输出制表符分隔的值。用
`--format table|tsv|csv|json` 选择，redis-cli 的 `--csv`、`--json` 也起作用。
`--no-header`、`--null <text>`、`--expanded`、`--timing` 对所有输出行的工具都有效。
表格格式里，控制字符和非 UTF-8 字节显示成 `\xHH`。

## 工具名和服务端命令

工具只认精确的小写名字，所以 `kevy-cli watch …`、`kevy-cli dump …`、
`kevy-cli restore …` 运行的是工具，而 redis-cli 会发送 `WATCH`、`DUMP`、`RESTORE`。
要发给服务端，就把命令写成大写：`kevy-cli -p 6004 DUMP mykey`。

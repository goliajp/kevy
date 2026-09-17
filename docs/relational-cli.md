# Relational commands in kevy-cli

kevy-cli is redis-cli for everything redis-cli does. Beside that it has tools
for the relational side of kevy — [tables](tables.md), [indexes](indexes.md)
and [views](views.md): reading the catalog as rows, running queries page by
page, dumping and restoring declarations, CSV in and out.

A tool runs behind `--kevy`, after the connection options; everything after
the tool name is the tool's. Without `--kevy`, the first word is a server
command, exactly as in redis-cli:

```sh
kevy-cli -p 6004 --kevy tables     # the tool
kevy-cli -p 6004 TABLE.LIST        # the command, printed as redis-cli prints it
```

The tools use the same connection options (`-h`, `-p`, `-s`, `-u`, `-a`,
`--user`, `-n`, `-2`/`-3`), so they authenticate and select a database exactly
as a command does.

## Reading the catalog

`tables`, `indexes` and `views` list the catalogs as rows; each takes a glob
on the name, and `indexes` also takes a table name. `describe` takes a table,
an index or a view:

```sh
kevy-cli -p 6004 --kevy tables 'user*'
kevy-cli -p 6004 --kevy indexes users
kevy-cli -p 6004 --kevy describe users      # columns, their types, the paths that read each
kevy-cli -p 6004 --kevy describe+ users     # the same, then TABLE.VERIFY
```

For a table, `describe` shows its prefix and primary key, every column with its
declared type and the compiled paths that read it, and each access path's build
state. For an index it shows the fields, stored values and the table that
compiled it; for a view, its composition tree and order. The columns come from
`TABLE.DESCRIBE`, which a server older than 6.5 does not have.

## Queries

`query` runs an `IDX.QUERY` or `VIEW.QUERY` and prints its rows. `--all`
follows the cursor page by page and stops at `--max-rows` (10,000 by default)
on a page boundary, saying which cursor to continue from:

```sh
kevy-cli -p 6004 --kevy query --all IDX.QUERY users.age RANGE 18 65 FIELDS name
kevy-cli -p 6004 --kevy explain users.age RANGE 18 65
kevy-cli -p 6004 --kevy explain --analyze IDX.QUERY users.age RANGE 18 65
kevy-cli -p 6004 --kevy advise
kevy-cli -p 6004 --kevy sql run "SELECT id, name FROM users WHERE age BETWEEN 18 AND 65"
```

`explain --analyze` runs the query and reports what this client measured —
round trips, pages, rows, wall time — not a server-side profile.

`sql run` takes one `SELECT`, plans it on the client against the declared
tables and sends the `IDX.QUERY` that answers it; it prints the selected
columns only. The server never sees SQL. A query no declared path can serve is
refused with the same text `kevy-cli --kevy sql plan` gives it, naming the index to
declare — no tool here fills a `WHERE` in by scanning.

## Declarations as files

```sh
kevy-cli -p 6004 --kevy show-create users                # the TABLE.DECLARE that recreates it
kevy-cli -p 6004 --kevy show-create users --as sql       # CREATE TABLE / CREATE INDEX
kevy-cli -p 6004 --kevy dump --schema > schema.kevy      # tables, then indexes, then views
kevy-cli -p 6005 --kevy run -f schema.kevy               # replay it on another server
kevy-cli -p 6004 --kevy dump --all ./dump                # schema plus each table's rows as CSV
kevy-cli -p 6005 --kevy load ./dump                   # rows, declarations, wait-ready, doctor
```

The kevy form is one command per line, quoted so that `run -f` reads back the
same words. The SQL form is what `kevy-cli --kevy sql compile` turns into the same
declaration; what SQL has no words for — a key prefix other than `<table>:`,
`WINDOW`, `AUTODECLARE` — is kept as a `-- not carried by SQL` comment rather
than dropped. An index that a table compiled has no declaration of its own:
`show-create users.age` names the table instead.

`dump --all` writes into a new or empty directory: `schema.kevy`, `tables` and
one `table-N.csv` per table. Only the declared columns of rows under a table's
prefix are in it; `kevy-cli --kevy export` is the byte-for-byte copy of a keyspace.
`load` imports the rows first and declares afterwards, so each index is built
once from the rows instead of updated on every write. (`restore --from … --to …`
is the offline backup restore, a different level.)

## CSV in and out

```sh
kevy-cli -p 6004 --kevy import-csv users.csv --table users --header
kevy-cli -p 6004 --kevy import-csv users.csv --prefix user: --pk id --columns id,name,age
kevy-cli -p 6004 --kevy export-csv --table users users.csv
kevy-cli -p 6004 --kevy export-csv --table users --via "IDX.QUERY users.age RANGE 18 65" -
```

`import-csv` writes one hash per record, pipelined in batches of 512, and keeps
its place in `<file>.progress` for `--resume`. An empty cell writes no field: a
missing field is NULL. With `--table` the prefix, key and columns come from the
declaration and only declared columns are written; `--key-column` takes each
whole key from a column, as `export-csv` writes it.

`export-csv` writes RFC 4180 CSV, key first. Without `--via` it finds the keys
with `SCAN`, which walks the whole keyspace; with `--via` it pages the query.

## Scripts, waiting and watching

```sh
kevy-cli -p 6004 --kevy run -f migrate.kevy --atomic
kevy-cli -p 6004 --kevy wait-ready --table users --timeout 60
kevy-cli -p 6004 --kevy watch 2 IDX.LIST
kevy-cli -p 6004 --kevy status
kevy-cli -p 6004 --kevy feed follow --prefix user: --from tail --checkpoint feed.pos
```

`run` executes each line of `-f` files and `-c` commands and exits 0 when all
succeed, 1 on a usage error, 2 when the connection is lost and 3 when an error
reply stopped it (`--force` carries on). `--atomic` wraps the script in
`MULTI`/`EXEC` and says so: a command that fails does not undo the others.
`wait-ready` polls `IDX.LIST` until the chosen indexes have built.

## In the REPL

A line starting with a backslash is handled by kevy-cli, never sent: `\<tool>
[args]` runs any tool as `--kevy <tool>` would, and psql's short forms sit on
top:

| line | does |
|---|---|
| `\dt [pattern]` | `tables` |
| `\di [table\|pattern]` | `indexes` |
| `\dv [pattern]` | `views` |
| `\d name` / `\d+ name` | `describe` / `describe+` |
| `\query …` / `\dump --schema` / any `\<tool>` | that tool |
| `\watch seconds command…` | `watch` |
| `\i file` | `run -f file` |
| `\conninfo` | `status` |
| `\x` / `\timing` | expanded rows / elapsed time after each command, on or off |
| `\?` | this list |

Against a kevy server, Tab completes table, index and view names after the
commands that take them, and a table's columns after `FIELDS`, `FILTER`,
`SORT`, `DISTINCT` and `FACET` in an `IDX.QUERY` on one of its paths.

## Output formats

```sh
kevy-cli -p 6004 --kevy tables --format csv
kevy-cli -p 6004 --json --kevy tables
kevy-cli -p 6004 --kevy query IDX.QUERY users.age RANGE 18 65 --null NULL --expanded
```

On a terminal rows print as an aligned table; piped, as tab-separated values.
`--format table|tsv|csv|json` chooses, and redis-cli's `--csv` and `--json`
choose too. `--no-header`, `--null <text>`, `--expanded` and `--timing` apply
to every tool that prints rows. The table format shows control bytes and bytes
that are not UTF-8 as `\xHH`.

## Tool names and server commands

A bare word is always a server command, whatever its case: `watch`, `dump`
and `restore` are Redis and Valkey commands, `backup` and `digest` are Redis
8 commands, and the command table grows with every release. So kevy's tools
never take bare names; they live behind one option, `--kevy`, the way
redis-cli's own cluster manager lives behind `--cluster`.

The tools kevy-cli 6.4 shipped as bare words (`kevy-cli doctor -p 6004`,
`kevy-cli export …`, `kevy-cli sql compile …`) still run until 7.0, each
printing one line with its `--kevy` form. `backup` and `restore` are tools
only in their own flag shapes (`--data-dir`/`--to`, `--from`/`--to`);
`digest <prefix>` stays the tool until 7.0.

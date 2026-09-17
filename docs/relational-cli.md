# Relational commands in kevy-cli

kevy-cli is redis-cli for everything redis-cli does. Beside that it has tools
for the relational side of kevy — [tables](tables.md), [indexes](indexes.md)
and [views](views.md): reading the catalog as rows, running queries page by
page, dumping and restoring declarations, CSV in and out.

A tool runs when its exact lower-case name is the first word after the
connection options. Every other word is a server command, as in redis-cli:

```sh
kevy-cli -p 6004 tables            # the tool
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
kevy-cli -p 6004 tables 'user*'
kevy-cli -p 6004 indexes users
kevy-cli -p 6004 describe users      # columns, their types, the paths that read each
kevy-cli -p 6004 describe+ users     # the same, then TABLE.VERIFY
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
kevy-cli -p 6004 query --all IDX.QUERY users.age RANGE 18 65 FIELDS name
kevy-cli -p 6004 explain users.age RANGE 18 65
kevy-cli -p 6004 explain --analyze IDX.QUERY users.age RANGE 18 65
kevy-cli -p 6004 advise
kevy-cli -p 6004 sql run "SELECT id, name FROM users WHERE age BETWEEN 18 AND 65"
```

`explain --analyze` runs the query and reports what this client measured —
round trips, pages, rows, wall time — not a server-side profile.

`sql run` takes one `SELECT`, plans it on the client against the declared
tables and sends the `IDX.QUERY` that answers it; it prints the selected
columns only. The server never sees SQL. A query no declared path can serve is
refused with the same text `kevy-cli sql plan` gives it, naming the index to
declare — no tool here fills a `WHERE` in by scanning.

## Declarations as files

```sh
kevy-cli -p 6004 show-create users                # the TABLE.DECLARE that recreates it
kevy-cli -p 6004 show-create users --as sql       # CREATE TABLE / CREATE INDEX
kevy-cli -p 6004 dump --schema > schema.kevy      # tables, then indexes, then views
kevy-cli -p 6005 run -f schema.kevy               # replay it on another server
kevy-cli -p 6004 dump --all ./dump                # schema plus each table's rows as CSV
kevy-cli -p 6005 restore ./dump                   # rows, declarations, wait-ready, doctor
```

The kevy form is one command per line, quoted so that `run -f` reads back the
same words. The SQL form is what `kevy-cli sql compile` turns into the same
declaration; what SQL has no words for — a key prefix other than `<table>:`,
`WINDOW`, `AUTODECLARE` — is kept as a `-- not carried by SQL` comment rather
than dropped. An index that a table compiled has no declaration of its own:
`show-create users.age` names the table instead.

`dump --all` writes into a new or empty directory: `schema.kevy`, `tables` and
one `table-N.csv` per table. Only the declared columns of rows under a table's
prefix are in it; `kevy-cli export` is the byte-for-byte copy of a keyspace.
`restore` loads the rows first and declares afterwards, so each index is built
once from the rows instead of updated on every write. `restore --from … --to …`
is still the offline backup restore.

## CSV in and out

```sh
kevy-cli -p 6004 import-csv users.csv --table users --header
kevy-cli -p 6004 import-csv users.csv --prefix user: --pk id --columns id,name,age
kevy-cli -p 6004 export-csv --table users users.csv
kevy-cli -p 6004 export-csv --table users --via "IDX.QUERY users.age RANGE 18 65" -
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
kevy-cli -p 6004 run -f migrate.kevy --atomic
kevy-cli -p 6004 wait-ready --table users --timeout 60
kevy-cli -p 6004 watch 2 IDX.LIST
kevy-cli -p 6004 status
kevy-cli -p 6004 feed follow --prefix user: --from tail --checkpoint feed.pos
```

`run` executes each line of `-f` files and `-c` commands and exits 0 when all
succeed, 1 on a usage error, 2 when the connection is lost and 3 when an error
reply stopped it (`--force` carries on). `--atomic` wraps the script in
`MULTI`/`EXEC` and says so: a command that fails does not undo the others.
`wait-ready` polls `IDX.LIST` until the chosen indexes have built.

## In the REPL

A line starting with a backslash is handled by kevy-cli, never sent:

| line | does |
|---|---|
| `\dt [pattern]` | `tables` |
| `\di [table\|pattern]` | `indexes` |
| `\dv [pattern]` | `views` |
| `\d name` / `\d+ name` | `describe` / `describe+` |
| `\query …` / `\explain …` / `\advise` | `query` / `explain` / `advise` |
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
kevy-cli -p 6004 tables --format csv
kevy-cli -p 6004 --json tables
kevy-cli -p 6004 query IDX.QUERY users.age RANGE 18 65 --null NULL --expanded
```

On a terminal rows print as an aligned table; piped, as tab-separated values.
`--format table|tsv|csv|json` chooses, and redis-cli's `--csv` and `--json`
choose too. `--no-header`, `--null <text>`, `--expanded` and `--timing` apply
to every tool that prints rows. The table format shows control bytes and bytes
that are not UTF-8 as `\xHH`.

## Tool names and server commands

Tools are recognised by their exact lower-case names, so `kevy-cli watch …`,
`kevy-cli dump …` and `kevy-cli restore …` run tools where redis-cli would send
`WATCH`, `DUMP` and `RESTORE`. Write the command in upper case to send it to
the server: `kevy-cli -p 6004 DUMP mykey`.

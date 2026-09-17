# kevy-cli のリレーショナルコマンド

redis-cli にできることは、kevy-cli にもすべてできます。それに加えて、kevy の
リレーショナルな側面——[テーブル](tables.md)、[インデックス](indexes.md)、
[ビュー](views.md)——を扱うツールがあります。カタログを行として読む、クエリをページ単位で
実行する、宣言をダンプして復元する、CSV を入出力する、といった用途です。

接続オプションの後の最初の単語がツール名（小文字で完全一致）ならツールが動きます。それ以外の
単語はすべてサーバーコマンドとして扱われ、redis-cli と同じです：

```sh
kevy-cli -p 6004 tables            # ツール
kevy-cli -p 6004 TABLE.LIST        # コマンド。redis-cli と同じ形式で表示
```

ツールは同じ接続オプション（`-h`、`-p`、`-s`、`-u`、`-a`、`--user`、`-n`、`-2`/`-3`）を
使うので、認証やデータベース選択はコマンドを送るときとまったく同じです。

## カタログを読む

`tables`、`indexes`、`views` は各カタログを行として一覧します。どれも名前のグロブを
受け付け、`indexes` はテーブル名も受け付けます。`describe` はテーブル、インデックス、
ビューのどれでも受け付けます：

```sh
kevy-cli -p 6004 tables 'user*'
kevy-cli -p 6004 indexes users
kevy-cli -p 6004 describe users      # 列、その型、その列を読むパス
kevy-cli -p 6004 describe+ users     # 同じ内容のあと TABLE.VERIFY
```

テーブルに対しては、プレフィックスと主キー、各列の宣言された型とその列を読むコンパイル済み
パス、各アクセスパスの構築状態を表示します。インデックスに対してはフィールド、保存値、
それをコンパイルしたテーブルを、ビューに対しては合成ツリーと並び順を表示します。列の情報は
`TABLE.DESCRIBE` から取得するため、6.5 より前のサーバーでは使えません。

## クエリ

`query` は `IDX.QUERY` または `VIEW.QUERY` を実行して行を表示します。`--all` を付けると
カーソルをたどってページごとに取得し、`--max-rows`（既定 10,000）に達するとページの
区切りで止まって、続きを取るカーソルを示します：

```sh
kevy-cli -p 6004 query --all IDX.QUERY users.age RANGE 18 65 FIELDS name
kevy-cli -p 6004 explain users.age RANGE 18 65
kevy-cli -p 6004 explain --analyze IDX.QUERY users.age RANGE 18 65
kevy-cli -p 6004 advise
kevy-cli -p 6004 sql run "SELECT id, name FROM users WHERE age BETWEEN 18 AND 65"
```

`explain --analyze` はクエリを実際に実行し、このクライアントが計測した値——往復回数、
ページ数、行数、経過時間——を報告します。サーバー側のプロファイルではありません。

`sql run` は `SELECT` を 1 文受け取り、宣言済みのテーブルに対してクライアント側で計画を
立て、それに答える `IDX.QUERY` を送ります。表示するのは SELECT で指定した列だけです。
サーバーが SQL を見ることはありません。宣言済みのどのパスでも答えられないクエリは、
`kevy-cli sql plan` と同じ文面で拒否され、宣言すべきインデックスが示されます——ここに、
スキャンで `WHERE` を補うツールはありません。

## 宣言をファイルにする

```sh
kevy-cli -p 6004 show-create users                # それを再作成する TABLE.DECLARE
kevy-cli -p 6004 show-create users --as sql       # CREATE TABLE / CREATE INDEX
kevy-cli -p 6004 dump --schema > schema.kevy      # テーブル、インデックス、ビューの順
kevy-cli -p 6005 run -f schema.kevy               # 別のサーバーで再実行
kevy-cli -p 6004 dump --all ./dump                # スキーマと各テーブルの行（CSV）
kevy-cli -p 6005 restore ./dump                   # 行、宣言、wait-ready、doctor
```

kevy 形式は 1 行 1 コマンドで、`run -f` が同じ単語として読み戻せるようにクォートされます。
SQL 形式は `kevy-cli sql compile` が同じ宣言に戻せる SQL です。SQL で表せない部分——
`<table>:` 以外のキープレフィックス、`WINDOW`、`AUTODECLARE`——は捨てずに
`-- not carried by SQL` コメントとして残します。テーブルがコンパイルしたインデックスには
独自の宣言がないため、`show-create users.age` はそのテーブルを示します。

`dump --all` は新しいディレクトリか空のディレクトリにだけ書き込みます：`schema.kevy`、
`tables`、テーブルごとの `table-N.csv` です。含まれるのはテーブルのプレフィックス下の行の、
宣言された列だけです。キー空間をバイト単位でそのまま写すには `kevy-cli export` を使います。
`restore` は先に行を取り込み、後で宣言するので、各インデックスは書き込みのたびに更新される
のではなく、行から一度だけ構築されます。`restore --from … --to …` は従来どおり
オフラインのバックアップ復元です。

## CSV の入出力

```sh
kevy-cli -p 6004 import-csv users.csv --table users --header
kevy-cli -p 6004 import-csv users.csv --prefix user: --pk id --columns id,name,age
kevy-cli -p 6004 export-csv --table users users.csv
kevy-cli -p 6004 export-csv --table users --via "IDX.QUERY users.age RANGE 18 65" -
```

`import-csv` は 1 レコードにつき 1 つの hash を書き、512 件ずつパイプラインで送ります。
進捗は `<file>.progress` に記録され、`--resume` でそこから再開します。空のセルは
フィールドを書きません。欠けたフィールドが NULL です。`--table` を付けるとプレフィックス、
主キー、列をテーブルの宣言から取り、宣言された列だけを書きます。`--key-column` は、
`export-csv` が書き出す列のように、ある列からキー全体をそのまま取ります。

`export-csv` は RFC 4180 形式の CSV を、キーを先頭列にして書きます。`--via` がなければ
`SCAN` でキーを探すのでキー空間全体をたどります。`--via` があればそのクエリをページ単位で
取得します。

## スクリプト、待機、監視

```sh
kevy-cli -p 6004 run -f migrate.kevy --atomic
kevy-cli -p 6004 wait-ready --table users --timeout 60
kevy-cli -p 6004 watch 2 IDX.LIST
kevy-cli -p 6004 status
kevy-cli -p 6004 feed follow --prefix user: --from tail --checkpoint feed.pos
```

`run` は `-f` のファイルと `-c` のコマンドを 1 行ずつ実行し、すべて成功すれば 0、
使い方の誤りなら 1、接続が切れたら 2、エラー応答で止まったら 3 で終了します（`--force`
なら続行）。`--atomic` はスクリプトを `MULTI`/`EXEC` で包み、失敗したコマンドが他を
取り消さないことを明示します。`wait-ready` は指定したインデックスの構築が終わるまで
`IDX.LIST` をポーリングします。

## REPL では

バックスラッシュで始まる行は kevy-cli が処理し、サーバーには送りません：

| 行 | 動作 |
|---|---|
| `\dt [pattern]` | `tables` |
| `\di [table\|pattern]` | `indexes` |
| `\dv [pattern]` | `views` |
| `\d name` / `\d+ name` | `describe` / `describe+` |
| `\query …` / `\explain …` / `\advise` | `query` / `explain` / `advise` |
| `\watch seconds command…` | `watch` |
| `\i file` | `run -f file` |
| `\conninfo` | `status` |
| `\x` / `\timing` | 展開表示 / コマンドごとの経過時間の切り替え |
| `\?` | この一覧 |

kevy サーバーに接続していれば、Tab キーで、名前を受け取るコマンドの後にテーブル、
インデックス、ビューの名前を補完します。テーブルのパスに対する `IDX.QUERY` の中では、
`FIELDS`、`FILTER`、`SORT`、`DISTINCT`、`FACET` の後にそのテーブルの列名を補完します。

## 出力形式

```sh
kevy-cli -p 6004 tables --format csv
kevy-cli -p 6004 --json tables
kevy-cli -p 6004 query IDX.QUERY users.age RANGE 18 65 --null NULL --expanded
```

端末に出力するときは行を揃えた表として、パイプに流すときはタブ区切りで出力します。
`--format table|tsv|csv|json` で選べ、redis-cli の `--csv`、`--json` も有効です。
`--no-header`、`--null <text>`、`--expanded`、`--timing` は行を出力するすべてのツールで
使えます。表形式では、制御文字と UTF-8 でないバイトを `\xHH` で表示します。

## ツール名とサーバーコマンド

ツールは小文字の名前の完全一致で認識されるため、`kevy-cli watch …`、`kevy-cli dump …`、
`kevy-cli restore …` はツールを実行します。redis-cli ならここで `WATCH`、`DUMP`、
`RESTORE` を送ります。サーバーに送るには、コマンドを大文字で書いてください：
`kevy-cli -p 6004 DUMP mykey`。

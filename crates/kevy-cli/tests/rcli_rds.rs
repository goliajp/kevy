//! kevy-cli's relational commands against a real kevy server.
//!
//! There is no reference client for these; the expected outputs are written
//! from the verbs' replies (TABLE.LIST, IDX.QUERY, FEED.READ, …) and the
//! formats the tools promise, not captured from the tools.

use std::io::Write;
use std::process::{Child, Command, Stdio};

struct Srv {
    child: Child,
    port: u16,
    dir: std::path::PathBuf,
}

impl Srv {
    fn start(feed: bool) -> Srv {
        let port = kevy_testnet::free_port();
        let bin =
            std::path::Path::new(env!("CARGO_BIN_EXE_kevy-cli")).parent().unwrap().join("kevy");
        if !bin.exists() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = Command::new(cargo)
                .args(["build", "-p", "kevy", "--bin", "kevy"])
                .status()
                .expect("spawn cargo build");
            assert!(status.success(), "cargo build -p kevy --bin kevy failed");
        }
        let dir = std::env::temp_dir().join(format!("kevy-rds-{port}"));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cmd = Command::new(&bin);
        cmd.args(["--port", &port.to_string(), "--threads", "2", "--no-aof"])
            .args(["--dir", dir.to_str().unwrap()]);
        if feed {
            let conf = dir.join("kevy.toml");
            std::fs::write(&conf, "[feed]\nenabled = true\n").unwrap();
            cmd.args(["--config", conf.to_str().unwrap()]);
        }
        let child =
            cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn().expect("spawn kevy server");
        kevy_testnet::assert_listening(port, "the server under test");
        Srv { child, port, dir }
    }

    fn port(&self) -> String {
        self.port.to_string()
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        let pid = self.child.id().to_string();
        let _ = Command::new("kill").args(["-TERM", &pid]).status(); // may have exited already
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while matches!(self.child.try_wait(), Ok(None)) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

fn cli(args: &[&str], stdin: &[u8], env: &[(&str, &str)]) -> Out {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kevy-cli"));
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.env_remove("FAKETTY").env("TERM", "dumb");
    cmd.envs(env.iter().copied());
    let mut child = cmd.spawn().expect("run kevy-cli");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    Out {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().unwrap_or(-1),
    }
}

/// Run `words` on the server, expecting success.
fn server(p: &str, words: &[&str]) {
    let out = cli(&[&["-p", p][..], words].concat(), b"", &[]);
    assert!(
        !out.stdout.starts_with("ERR") && out.code == 0,
        "{words:?}: {}{}",
        out.stdout,
        out.stderr
    );
}

/// A table `users` over `user:` with an age index and five rows, ready.
fn users(p: &str) {
    server(
        p,
        &[
            "TABLE.DECLARE",
            "users",
            "PREFIX",
            "user:",
            "PK",
            "id",
            "COLUMN",
            "id",
            "i64",
            "COLUMN",
            "name",
            "str",
            "COLUMN",
            "age",
            "i64",
            "INDEX",
            "age",
            "range",
        ],
    );
    for i in 1..=5 {
        server(
            p,
            &[
                "HSET",
                &format!("user:{i}"),
                "id",
                &i.to_string(),
                "name",
                &format!("n {i}"),
                "age",
                &(20 + i).to_string(),
            ],
        );
    }
    let ready = cli(&["-p", p, "wait-ready", "--table", "users", "--timeout", "10"], b"", &[]);
    assert_eq!((ready.stdout.lines().last(), ready.code), (Some("ready"), 0), "{}", ready.stderr);
}

#[test]
fn catalogs_describe_and_formats() {
    let s = Srv::start(false);
    let p = s.port();
    users(&p);
    let tables = cli(&["-p", &p, "tables"], b"", &[]);
    assert_eq!(
        tables.stdout,
        "name\tprefix\tpk\tcolumns\tindexes\torderpaths\twindow\nusers\tuser:\tid\t3\t1\t0\t-\n"
    );
    let json = cli(&["-p", &p, "--json", "tables", "us*"], b"", &[]);
    assert_eq!(
        json.stdout,
        "[{\"name\":\"users\",\"prefix\":\"user:\",\"pk\":\"id\",\"columns\":\"3\",\"indexes\":\"1\",\"orderpaths\":\"0\",\"window\":\"-\"}]\n"
    );
    assert_eq!(cli(&["-p", &p, "tables", "nope*", "--no-header"], b"", &[]).stdout, "");
    let indexes = cli(&["-p", &p, "indexes", "users", "--format", "csv"], b"", &[]);
    assert!(indexes.stdout.starts_with("name,table,prefix,kind,state,entries,bytes,hits,last_hit,auto\r\nusers.age,users,user:,range,ready,5,"), "{}", indexes.stdout);
    let table = cli(&["-p", &p, "describe+", "users"], b"", &[("FAKETTY", "1")]);
    assert!(
        table.stdout.starts_with(
            "Table \"users\"\n prefix | pk | autodeclare | window\n--------+----+-------------+--------\n user:  | id | 0           | -\n(1 row)\nColumns\n column | type | key | paths\n"
        ),
        "{}",
        table.stdout
    );
    assert!(table.stdout.contains("\n age    | i64  |     | users.age\n(3 rows)\nAccess paths\n name      | prefix | kind  | state | entries |"), "{}", table.stdout);
    assert!(table.stdout.contains("\nVerification\n index     | entries |"), "{}", table.stdout);
    let missing = cli(&["-p", &p, "describe", "nope"], b"", &[]);
    assert_eq!(
        (missing.stderr.as_str(), missing.code),
        ("kevy-cli: no table, index or view named 'nope'\n", 1)
    );
    let expanded = cli(&["-p", &p, "views", "--expanded", "--format", "table"], b"", &[]);
    assert_eq!(expanded.stdout, "");
    let bad = cli(&["-p", &p, "tables", "--format", "xml"], b"", &[]);
    assert_eq!(
        (bad.stderr.as_str(), bad.code),
        ("kevy-cli: --format must be table, tsv, csv or json, not 'xml'\n", 1)
    );
}

#[test]
fn queries_page_explain_and_advise() {
    let s = Srv::start(false);
    let p = s.port();
    users(&p);
    let all = cli(
        &[
            "-p",
            &p,
            "query",
            "--all",
            "IDX.QUERY",
            "users.age",
            "RANGE",
            "0",
            "100",
            "LIMIT",
            "2",
            "FIELDS",
            "name",
        ],
        b"",
        &[],
    );
    assert_eq!(
        all.stdout,
        "key\tvalue\tname\nuser:1\t21\tn 1\nuser:2\t22\tn 2\nuser:3\t23\tn 3\nuser:4\t24\tn 4\nuser:5\t25\tn 5\n"
    );
    let capped = cli(
        &[
            "-p",
            &p,
            "query",
            "--all",
            "--max-rows",
            "3",
            "IDX.QUERY",
            "users.age",
            "RANGE",
            "0",
            "100",
            "LIMIT",
            "2",
        ],
        b"",
        &[],
    );
    assert_eq!(capped.stdout.lines().count(), 5, "{}", capped.stdout);
    assert!(
        capped
            .stderr
            .starts_with("kevy-cli: stopped after 4 rows (--max-rows); continue with CURSOR ")
    );
    let refused = cli(&["-p", &p, "query", "IDX.QUERY", "users.name", "EQ", "x"], b"", &[]);
    assert_eq!(refused.code, 1);
    assert!(
        refused.stderr.starts_with("(error) ERR no such index 'users.name'")
            && refused.stderr.contains("`kevy-cli advise`")
    );
    let plan = cli(&["-p", &p, "explain", "users.age", "RANGE", "0", "100"], b"", &[]);
    assert!(
        plan.stdout.starts_with("kind\tstate\test_rows\tplan\nrange\tready\t5\tsingle-index scan"),
        "{}",
        plan.stdout
    );
    let analyzed = cli(
        &[
            "-p",
            &p,
            "explain",
            "--analyze",
            "IDX.QUERY",
            "users.age",
            "RANGE",
            "0",
            "100",
            "LIMIT",
            "2",
        ],
        b"",
        &[],
    );
    assert!(analyzed.stdout.starts_with("Client-side measurement (round trips from this client; not a server-side breakdown)\nrows\tpages\telapsed_ms\n5\t3\t"), "{}", analyzed.stdout);
    let advice = cli(&["-p", &p, "advise"], b"", &[]);
    assert!(advice.stdout.starts_with("Refused queries would be served by\nhits\tname\tcommand\n1\tusers.name\tTABLE.DECLARE users"), "{}", advice.stdout);
    let usage = cli(&["-p", &p, "explain"], b"", &[]);
    assert_eq!(usage.code, 1);
    let view = cli(
        &[
            "-p",
            &p,
            "VIEW.CREATE",
            "young",
            "QUERY",
            "users.age",
            "RANGE",
            "0",
            "22",
            "ORDER",
            "BY",
            "users.age",
        ],
        b"",
        &[],
    );
    assert_eq!(view.stdout, "OK\n");
    let tree = cli(&["-p", &p, "explain", "view", "young"], b"", &[]);
    assert!(tree.stdout.contains("tree"), "{}", tree.stdout);
    let rows = cli(&["-p", &p, "query", "VIEW.QUERY", "young"], b"", &[]);
    assert!(rows.stdout.starts_with("key\torder_value\n"), "{}", rows.stdout);
}

#[test]
fn scripts_status_watch_and_waiting() {
    let s = Srv::start(false);
    let p = s.port();
    let dir = std::env::temp_dir().join(format!("kevy-rds-run-{}", s.port));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("script.txt");
    std::fs::write(&file, "SET a 1\n# a comment\n\nINCR a\nHGET\nGET a\n").unwrap();
    let f = file.to_str().unwrap();
    let stopped = cli(&["-p", &p, "run", "-f", f], b"", &[]);
    assert_eq!(
        (stopped.stdout.as_str(), stopped.code),
        ("OK\n2\nERR wrong number of arguments for 'hget' command\n\n", 3)
    );
    let forced = cli(&["-p", &p, "run", "-f", f, "--force", "--echo"], b"", &[]);
    assert!(
        forced
            .stdout
            .ends_with("HGET\nERR wrong number of arguments for 'hget' command\n\nGET a\n2\n")
            && forced.code == 3,
        "{}",
        forced.stdout
    );
    let atomic = cli(&["-p", &p, "run", "-c", "SET b 2", "-c", "GET b", "--atomic"], b"", &[]);
    assert_eq!((atomic.stdout.as_str(), atomic.code), ("OK\nQUEUED\nQUEUED\nOK\n2\n", 0));
    assert!(atomic.stderr.contains("does not undo the others"));
    assert_eq!(cli(&["-p", &p, "run", "-f", "/nonexistent/x"], b"", &[]).code, 1);
    assert_eq!(cli(&["-p", &p, "run", "-c", "SET \"a"], b"", &[]).code, 1);
    assert_eq!(cli(&["-p", &p, "run"], b"", &[]).code, 1);
    let status = cli(&["-p", &p, "status"], b"", &[]);
    assert!(
        status.stdout.starts_with("field\tvalue\nserver\tkevy\nversion\t"),
        "{}",
        status.stdout
    );
    assert!(
        status.stdout.contains(&format!("address\t127.0.0.1:{p}\nkeys\t2\n"))
            && status.stdout.ends_with("feed\toff\ntables\t0\nindexes\t0\nviews\t0\n")
    );
    let watched = cli(&["-p", &p, "watch", "0.05", "2", "GET", "b"], b"", &[]);
    assert_eq!(
        (watched.stdout.as_str(), watched.code),
        ("Every 0.05s: GET b\n\n2\nEvery 0.05s: GET b\n\n2\n", 0)
    );
    assert_eq!(cli(&["-p", &p, "watch", "x"], b"", &[]).code, 1);
    let late = cli(&["-p", &p, "wait-ready", "--index", "nosuch", "--timeout", "0.3"], b"", &[]);
    assert_eq!(
        (late.stdout.as_str(), late.stderr.as_str(), late.code),
        ("waiting for nosuch\n", "kevy-cli: wait-ready timed out\n", 1)
    );
    assert_eq!(cli(&["-p", &p, "wait-ready", "--bogus"], b"", &[]).code, 1);
    let lost = cli(&["-p", &p, "run", "-c", "SHUTDOWN NOSAVE", "-c", "GET a"], b"", &[]);
    assert_eq!(lost.code, 2, "{}{}", lost.stdout, lost.stderr);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn csv_in_and_out() {
    let s = Srv::start(false);
    let p = s.port();
    users(&p);
    let dir = std::env::temp_dir().join(format!("kevy-rds-csv-{}", s.port));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("more.csv");
    std::fs::write(
        &file,
        "id,name,age,note\r\n10,\"Ann, B\",30,\n11,Bo,,\"x \"\"q\"\"\"\n,skip,1,\n12,Cy,41,NULL\n",
    )
    .unwrap();
    let f = file.to_str().unwrap();
    let imported = cli(
        &[
            "-p",
            &p,
            "import-csv",
            f,
            "--prefix",
            "user:",
            "--pk",
            "id",
            "--header",
            "--null-marker",
            "NULL",
        ],
        b"",
        &[],
    );
    assert_eq!(
        (imported.stdout.as_str(), imported.code),
        ("imported 3 rows into user: (0 errors)\n", 0)
    );
    assert!(imported.stderr.contains("indexes are declared on 'user:'"));
    assert_eq!(
        std::fs::read_to_string(dir.join("more.csv.progress")).unwrap(),
        std::fs::metadata(&file).unwrap().len().to_string()
    );
    let bo = cli(&["-p", &p, "HMGET", "user:11", "id", "name", "age", "note"], b"", &[]);
    assert_eq!(bo.stdout, "11\nBo\n\nx \"q\"\n");
    let again = cli(
        &["-p", &p, "import-csv", f, "--prefix", "user:", "--pk", "id", "--header", "--resume"],
        b"",
        &[],
    );
    assert_eq!(again.stdout, "imported 0 rows into user: (0 errors)\n");
    let cols = cli(
        &["-p", &p, "import-csv", f, "--prefix", "x:", "--pk", "id", "--columns", "id,name"],
        b"",
        &[],
    );
    assert_eq!(cols.stdout, "imported 4 rows into x: (0 errors)\n");
    assert_eq!(
        cli(&["-p", &p, "import-csv", f, "--prefix", "x:", "--pk", "nope", "--header"], b"", &[])
            .code,
        1
    );
    assert_eq!(cli(&["-p", &p, "import-csv", f, "--prefix", "x:"], b"", &[]).code, 1);
    let via = cli(
        &[
            "-p",
            &p,
            "export-csv",
            "--via",
            "IDX.QUERY users.age RANGE 25 50",
            "--columns",
            "name,age",
            "-",
        ],
        b"",
        &[],
    );
    assert_eq!(
        (via.stdout.as_str(), via.stderr.as_str()),
        (
            "key,name,age\r\nuser:5,n 5,25\r\nuser:10,\"Ann, B\",30\r\nuser:12,Cy,41\r\n",
            "exported 3 rows\n"
        )
    );
    let out = dir.join("out.csv");
    let scanned = cli(
        &["-p", &p, "export-csv", "--prefix", "user:1", "--columns", "name", out.to_str().unwrap()],
        b"",
        &[],
    );
    assert!(scanned.stderr.contains("walks the whole keyspace with SCAN") && scanned.code == 0);
    let mut lines: Vec<String> =
        std::fs::read_to_string(&out).unwrap().split("\r\n").map(str::to_string).collect();
    lines.sort();
    assert_eq!(
        lines,
        ["", "key,name", "user:1,n 1", "user:10,\"Ann, B\"", "user:11,Bo", "user:12,Cy"]
    );
    assert_eq!(cli(&["-p", &p, "export-csv", "--columns", "a", "-"], b"", &[]).code, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A bare index and a view beside `users`, so every catalog has an entry.
fn users_index_and_view(p: &str) {
    users(p);
    server(
        p,
        &[
            "IDX.CREATE",
            "city",
            "ON",
            "PREFIX",
            "user:",
            "FIELD",
            "city",
            "TYPE",
            "str",
            "KIND",
            "unique",
        ],
    );
    server(
        p,
        &[
            "VIEW.CREATE",
            "young",
            "QUERY",
            "users.age",
            "RANGE",
            "0",
            "22",
            "ORDER",
            "BY",
            "users.age",
        ],
    );
    let ready = cli(&["-p", p, "wait-ready", "--all", "--timeout", "10"], b"", &[]);
    assert_eq!(ready.code, 0, "{}", ready.stderr);
}

const USERS_DECLARATION: &str = "TABLE.DECLARE users PREFIX user: PK id COLUMN id i64 COLUMN name str COLUMN age i64 INDEX age range";

#[test]
fn describe_and_show_create_read_declarations_back() {
    let s = Srv::start(false);
    let p = s.port();
    users_index_and_view(&p);
    let table = cli(&["-p", &p, "describe", "users"], b"", &[]);
    assert!(
        table.stdout.starts_with(
            "Table \"users\"\nprefix\tpk\tautodeclare\twindow\nuser:\tid\t0\t-\n\
             Columns\ncolumn\ttype\tkey\tpaths\nid\ti64\tpk\t\nname\tstr\t\t\nage\ti64\t\tusers.age\n\
             Access paths\nname\tprefix\tkind\tstate\tentries\tbytes\thits\tlast_hit\tauto\nusers.age\tuser:\trange\tready\t5\t"
        ),
        "{}",
        table.stdout
    );
    let index = cli(&["-p", &p, "describe", "city"], b"", &[]);
    assert_eq!(
        index.stdout,
        "Index \"city\"\nprefix\tkind\ttype\ttable\tpositions\tmaxmem\tgroupby\tann\nuser:\tunique\tstr\t-\t0\t0\t-\t-\nFields\nfield\tweight\ncity\t1\n"
    );
    let view = cli(&["-p", &p, "describe", "young"], b"", &[]);
    assert_eq!(
        view.stdout,
        "View \"young\"\norder_by\tdesc\tmode\ttopk\tvia\tquery\nusers.age\t0\tvirtual\t0\t-\tusers.age RANGE 0 22\n"
    );
    let kevy = cli(&["-p", &p, "show-create", "users"], b"", &[]);
    assert_eq!((kevy.stdout.as_str(), kevy.code), (format!("{USERS_DECLARATION}\n").as_str(), 0));
    let sql = cli(&["-p", &p, "show-create", "users", "--as", "sql"], b"", &[]);
    assert_eq!(
        sql.stdout,
        "CREATE TABLE users (\n    id bigint PRIMARY KEY,\n    name text,\n    age bigint\n);\n\
         CREATE INDEX ON users (age);\n-- not carried by SQL: PREFIX user:\n"
    );
    let compiled = cli(&["-p", &p, "show-create", "users.age"], b"", &[]);
    assert_eq!(
        (compiled.stderr.as_str(), compiled.code),
        (
            "kevy-cli: show-create: index 'users.age' is compiled by table 'users'; show-create users declares it\n",
            1
        )
    );
    let bare = cli(&["-p", &p, "show-create", "city", "--as", "sql"], b"", &[]);
    assert_eq!(
        (bare.stderr.as_str(), bare.code),
        (
            "kevy-cli: show-create: an index has no SQL form here (SQL indexes and views compile from a table); --as kevy prints it\n",
            1
        )
    );
    let bad = cli(&["-p", &p, "show-create", "users", "--as", "yaml"], b"", &[]);
    assert_eq!(
        (bad.stderr.as_str(), bad.code),
        ("kevy-cli: show-create: --as takes kevy or sql\n", 1)
    );
}

#[test]
fn dump_restore_and_sql_run() {
    let a = Srv::start(false);
    let b = Srv::start(false);
    let (pa, pb) = (a.port(), b.port());
    users_index_and_view(&pa);
    server(&pa, &["HSET", "user:3", "city", "kyoto", "nickname", "not declared"]);
    let schema = cli(&["-p", &pa, "dump", "--schema"], b"", &[]);
    assert_eq!(
        schema.stdout,
        format!(
            "# kevy-cli dump --schema: tables, indexes, views; replay with run -f\n{USERS_DECLARATION}\n\
             IDX.CREATE city ON PREFIX user: FIELD city TYPE str KIND unique\n\
             VIEW.CREATE young QUERY users.age RANGE 0 22 ORDER BY users.age\n"
        )
    );
    let only = cli(&["-p", &pa, "dump", "--schema", "--table", "users", "--as", "sql"], b"", &[]);
    assert!(
        only.stdout.starts_with(
            "-- kevy-cli dump --schema --as sql; compile with sql compile\nCREATE TABLE users ("
        ),
        "{}",
        only.stdout
    );
    assert!(!only.stdout.contains("city"), "--table leaves bare indexes out: {}", only.stdout);
    let dir = std::env::temp_dir().join(format!("kevy-rds-dump-{}", a.port));
    let _ = std::fs::remove_dir_all(&dir);
    let d = dir.to_str().unwrap();
    let dumped = cli(&["-p", &pa, "dump", "--all", d], b"", &[]);
    assert_eq!(
        (dumped.stdout.as_str(), dumped.code),
        (format!("dumped 1 table(s) to {d}\n").as_str(), 0),
        "{}",
        dumped.stderr
    );
    assert_eq!(std::fs::read_to_string(dir.join("tables")).unwrap(), "table-1.csv users\n");
    assert_eq!(cli(&["-p", &pa, "dump", "--all", d], b"", &[]).code, 1, "a dump never overwrites");
    let restored = cli(&["-p", &pb, "restore", d], b"", &[]);
    assert_eq!(restored.code, 0, "{}{}", restored.stdout, restored.stderr);
    assert!(
        restored.stdout.starts_with("imported 5 rows into user: (0 errors)\n"),
        "{}",
        restored.stdout
    );
    assert!(
        restored.stdout.contains("\nready\n")
            && restored.stdout.contains("doctor: 3 checked — 0 drifted"),
        "{}",
        restored.stdout
    );
    assert_eq!(cli(&["-p", &pb, "dump", "--schema"], b"", &[]).stdout, schema.stdout);
    // Declared columns travel; a field outside the declaration does not.
    let row =
        cli(&["-p", &pb, "HMGET", "user:3", "id", "name", "age", "city", "nickname"], b"", &[]);
    assert_eq!(row.stdout, "3\nn 3\n23\n\n\n");
    let rows = cli(
        &["-p", &pb, "sql", "run", "SELECT name, id FROM users WHERE age BETWEEN 22 AND 23"],
        b"",
        &[],
    );
    assert_eq!(
        (rows.stdout.as_str(), rows.code),
        ("name\tid\nn 2\t2\nn 3\t3\n", 0),
        "{}",
        rows.stderr
    );
    let refused =
        cli(&["-p", &pb, "sql", "run", "SELECT * FROM users WHERE name = 'n 1'"], b"", &[]);
    assert_eq!(
        (refused.stderr.as_str(), refused.code),
        (
            "kevy-cli: sql run: line 1, col 1: view 'select': WHERE (name EQ) matches no declared access path \u{2014} add: CREATE INDEX ON users (name)\n",
            1
        )
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn csv_by_table() {
    let s = Srv::start(false);
    let p = s.port();
    users(&p);
    let dir = std::env::temp_dir().join(format!("kevy-rds-table-csv-{}", s.port));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("in.csv");
    std::fs::write(&file, "id,name,age,extra\n9,nine,29,x\n").unwrap();
    let imported = cli(
        &["-p", &p, "import-csv", file.to_str().unwrap(), "--table", "users", "--header"],
        b"",
        &[],
    );
    assert_eq!(
        (imported.stdout.as_str(), imported.code),
        ("imported 1 rows into user: (0 errors)\n", 0)
    );
    let row = cli(&["-p", &p, "HMGET", "user:9", "id", "name", "age", "extra"], b"", &[]);
    assert_eq!(row.stdout, "9\nnine\n29\n\n", "only declared columns are written");
    let exported = cli(
        &[
            "-p",
            &p,
            "export-csv",
            "--table",
            "users",
            "--via",
            "IDX.QUERY users.age RANGE 25 29",
            "-",
        ],
        b"",
        &[],
    );
    assert_eq!(exported.stdout, "key,id,name,age\r\nuser:5,5,n 5,25\r\nuser:9,9,nine,29\r\n");
    let missing = cli(&["-p", &p, "export-csv", "--table", "nope", "-"], b"", &[]);
    assert_eq!(
        (missing.stderr.as_str(), missing.code),
        ("kevy-cli: export-csv: no table named 'nope'\n", 1)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Each shard's `(generation, offset)` tail.
fn tails(p: &str) -> Vec<(i64, i64)> {
    let shards: usize = cli(&["-p", p, "FEED.SHARDS"], b"", &[]).stdout.trim().parse().unwrap();
    (0..shards)
        .map(|n| {
            let out = cli(&["-p", p, "FEED.TAIL", &n.to_string()], b"", &[]).stdout;
            let mut it = out.lines().map(|l| l.parse::<i64>().unwrap());
            (it.next().unwrap(), it.next().unwrap())
        })
        .collect()
}

#[test]
fn feed_follow_reads_filters_and_remembers() {
    let s = Srv::start(true);
    let p = s.port();
    let before = tails(&p);
    server(&p, &["SET", "noise", "x"]);
    for i in 0..24 {
        server(&p, &["SET", &format!("user:{i}"), "1"]);
    }
    // A shard that took at least three user writes: every read below has
    // a frame to return, so no --limit waits for one.
    let after = tails(&p);
    let (shard, (generation, offset)) =
        before.iter().copied().enumerate().max_by_key(|(n, t)| after[*n].1 - t.1).unwrap();
    assert!(after[shard].1 - offset >= 4, "{before:?} {after:?}");
    let (sh, from) = (shard.to_string(), format!("{generation}:{offset}"));
    let one = cli(
        &[
            "-p", &p, "feed", "follow", "--shard", &sh, "--from", &from, "--prefix", "user:",
            "--limit", "2",
        ],
        b"",
        &[],
    );
    assert_eq!((one.stdout.lines().count(), one.code), (2, 0), "{}{}", one.stdout, one.stderr);
    assert!(
        one.stdout
            .starts_with(&format!("{{\"shard\":{shard},\"generation\":{generation},\"offset\":"))
    );
    assert!(one.stdout.contains("\"argv\":[\"SET\",\"user:"));
    let dir = std::env::temp_dir().join(format!("kevy-rds-feed-{}", s.port));
    std::fs::create_dir_all(&dir).unwrap();
    let ck = dir.join("ck");
    let first = cli(
        &[
            "-p",
            &p,
            "feed",
            "follow",
            "--shard",
            &sh,
            "--from",
            &from,
            "--limit",
            "1",
            "--checkpoint",
            ck.to_str().unwrap(),
        ],
        b"",
        &[],
    );
    assert_eq!(first.code, 0);
    assert_eq!(
        std::fs::read_to_string(&ck).unwrap(),
        format!("{shard} {generation} {}\n", offset + 1)
    );
    let resp = cli(
        &[
            "-p",
            &p,
            "feed",
            "follow",
            "--shard",
            &sh,
            "--checkpoint",
            ck.to_str().unwrap(),
            "--limit",
            "1",
            "--as",
            "resp",
        ],
        b"",
        &[],
    );
    assert!(resp.stdout.starts_with("*3\r\n$3\r\nSET\r\n"), "{}", resp.stdout);
    assert_eq!(
        std::fs::read_to_string(&ck).unwrap(),
        format!("{shard} {generation} {}\n", offset + 2)
    );
    let stale = cli(
        &[
            "-p",
            &p,
            "feed",
            "follow",
            "--shard",
            &sh,
            "--from",
            &format!("{}:0", generation + 1),
            "--limit",
            "1",
        ],
        b"",
        &[],
    );
    assert_eq!(stale.code, 3, "{}{}", stale.stdout, stale.stderr);
    assert!(stale.stderr.contains("needs a resync") && stale.stderr.contains("--on-resync jump"));
    assert_eq!(cli(&["-p", &p, "feed", "tail"], b"", &[]).code, 1);
    assert_eq!(cli(&["-p", &p, "feed", "follow", "--from", "1:2"], b"", &[]).code, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn repl_backslash_commands_and_doctor_scope() {
    let s = Srv::start(false);
    let p = s.port();
    users(&p);
    server(
        &p,
        &["IDX.CREATE", "bare", "ON", "PREFIX", "b:", "FIELD", "n", "TYPE", "i64", "KIND", "range"],
    );
    assert_eq!(
        cli(&["-p", &p, "wait-ready", "--index", "bare", "--timeout", "10"], b"", &[]).code,
        0
    );
    let dir = std::env::temp_dir().join(format!("kevy-rds-repl-{}", s.port));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("s.txt");
    std::fs::write(&script, "SET k v\nGET k\n").unwrap();
    let input = format!(
        "\\dt\n\\x\n\\x\n\\timing\nPING\n\\timing\n\\i {}\n\\foo\n\\\nMULTI\n\\di\nDISCARD\n\\?\n",
        script.display()
    );
    let repl = cli(&["-p", &p], input.as_bytes(), &[]);
    let want = "name\tprefix\tpk\tcolumns\tindexes\torderpaths\twindow\nusers\tuser:\tid\t3\t1\t0\t-\nExpanded display is on.\nExpanded display is off.\nTiming is on.\nPONG\nTime: ";
    assert!(repl.stdout.starts_with(want), "{}", repl.stdout);
    assert!(repl.stdout.contains("Timing is off.\nOK\nv\nInvalid command \\foo. Try \\? for help.\nInvalid command \\. Try \\? for help.\nOK\nOK\n\\dt [pattern]      tables\n"), "{}", repl.stdout);
    assert!(repl.stderr.contains("relational commands do not run inside MULTI"));
    let doctor = cli(&["doctor", "-p", &p, "--indexes", "--views"], b"", &[]);
    assert!(
        doctor.stdout.contains("  OK       users  (")
            && doctor.stdout.contains("  OK       index bare  ("),
        "{}",
        doctor.stdout
    );
    assert!(
        doctor.stdout.ends_with("doctor: 2 checked — 0 drifted, 0 warned, 0 still building\n")
            && doctor.code == 0
    );
    let tables_only = cli(&["doctor", "-p", &p], b"", &[]);
    assert!(
        tables_only
            .stdout
            .ends_with("doctor: 1 table(s) — 0 drifted, 0 warned, 0 still building\n")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

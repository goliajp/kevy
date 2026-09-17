//! The tools kevy-cli shipped before `--kevy`, reached through it on the
//! session's connection, and their deprecated bare forms (RFC §13).

use std::process::{Child, Command, Stdio};

struct Srv {
    child: Child,
    port: u16,
    dir: std::path::PathBuf,
}

impl Srv {
    fn start() -> Srv {
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
        let dir = std::env::temp_dir().join(format!("kevy-tools-{port}"));
        std::fs::create_dir_all(&dir).unwrap();
        let child = Command::new(&bin)
            .args(["--port", &port.to_string(), "--threads", "2", "--no-aof"])
            .args(["--dir", dir.to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kevy server");
        kevy_testnet::assert_listening(port, "the server under test");
        Srv { child, port, dir }
    }

    fn port(&self) -> String {
        self.port.to_string()
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        let _ = self.child.kill(); // may have exited already
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

fn cli(args: &[&str]) -> Out {
    let out = Command::new(env!("CARGO_BIN_EXE_kevy-cli"))
        .args(args)
        .env_remove("FAKETTY")
        .stdin(Stdio::null())
        .output()
        .expect("run kevy-cli");
    Out {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().unwrap_or(-1),
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("kevy-tools-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn three_rows(p: &str) {
    for i in 1..=3 {
        let set = cli(&["-p", p, "HSET", &format!("u:{i}"), "name", &format!("n{i}")]);
        assert_eq!(set.code, 0, "{}", set.stderr);
    }
}

#[test]
fn shipped_tools_run_under_kevy_on_the_session_connection() {
    let (a, b) = (Srv::start(), Srv::start());
    let (pa, pb) = (a.port(), b.port());
    three_rows(&pa);
    let digest = cli(&["-p", &pa, "--kevy", "digest", "u:"]);
    assert!(digest.stdout.starts_with("3 keys ") && digest.code == 0, "{}", digest.stderr);
    let other = format!("127.0.0.1:{pb}");
    let differs = cli(&["-p", &pa, "--kevy", "diff", &other, "u:"]);
    assert!(
        differs.stdout.contains("B: 0 keys")
            && differs.stdout.contains("MISMATCH")
            && differs.code == 1
    );
    let dir = scratch("stream");
    let file = dir.join("u.resp");
    let f = file.to_str().unwrap();
    assert_eq!(
        cli(&["-p", &pa, "--kevy", "export", "--prefix", "u:", f]).stdout,
        format!("exported 3 keys -> {f}\n")
    );
    assert!(cli(&["-p", &pb, "--kevy", "import", f]).stdout.starts_with("imported: "));
    let uri = format!("redis://127.0.0.1:{pb}");
    let same = cli(&["-p", &pa, "--kevy", "diff", &uri, "u:"]);
    assert!(same.stdout.ends_with("OK\n") && same.code == 0, "{}{}", same.stdout, same.stderr);
    assert_eq!(cli(&["-p", &pa, "--kevy", "copy-prefix", "u:", "v:"]).stdout, "copied 3 keys\n");
    assert_eq!(
        cli(&["-p", &pa, "--kevy", "delete-prefix", "--dry-run", "v:"]).stdout,
        "would delete 3 keys\n"
    );
    assert_eq!(cli(&["-p", &pa, "--kevy", "inspect", "u:"]).code, 0);
    let names = cli(&["-p", &pa, "--kevy", "backfill-keys", "--from-prefix", "u:"]);
    let mut listed: Vec<&str> = names.stdout.lines().collect();
    listed.sort_unstable(); // SCAN order
    assert_eq!(listed, ["1", "2", "3"], "{}", names.stderr);
    let doctor = cli(&["-p", &pa, "--kevy", "doctor"]);
    assert_eq!(doctor.stdout, "doctor: no tables declared — nothing to verify\n");
    let schema = dir.join("s.sql");
    std::fs::write(&schema, "CREATE TABLE t (id bigint PRIMARY KEY, name text);\n").unwrap();
    let applied =
        cli(&["-p", &pa, "--kevy", "sql", "compile", schema.to_str().unwrap(), "--apply"]);
    assert_eq!(
        (applied.stdout.as_str(), applied.code),
        ("TABLE.DECLARE t → OK\n", 0),
        "{}",
        applied.stderr
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_backup_pair_and_sql_plan_need_no_server() {
    let closed = kevy_testnet::free_port().to_string();
    let dir = scratch("offline");
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("shards.meta"), "1\nkevyhash\n").unwrap();
    let packed = dir.join("b.kevybkp");
    let backup = cli(&[
        "-p",
        &closed,
        "--kevy",
        "backup",
        "--data-dir",
        data.to_str().unwrap(),
        "--to",
        packed.to_str().unwrap(),
    ]);
    assert_eq!(backup.code, 0, "{}", backup.stderr);
    let back = dir.join("back");
    let restore = cli(&[
        "-p",
        &closed,
        "--kevy",
        "restore",
        "--from",
        packed.to_str().unwrap(),
        "--to",
        back.to_str().unwrap(),
    ]);
    assert_eq!(restore.code, 0, "{}", restore.stderr);
    assert_eq!(std::fs::read(back.join("shards.meta")).unwrap(), b"1\nkevyhash\n");
    let schema = dir.join("s.sql");
    std::fs::write(&schema, "CREATE TABLE t (id bigint PRIMARY KEY);\n").unwrap();
    let plan = cli(&["-p", &closed, "--kevy", "sql", "plan", schema.to_str().unwrap()]);
    assert!(
        plan.stdout.starts_with("1 table(s) to declare:\n  t\n") && plan.code == 0,
        "{}",
        plan.stderr
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn shadow_and_lint_read_a_live_server() {
    let s = Srv::start();
    let p = s.port();
    for (owner, members) in [("mailbox:a", ["m1", "m2"]), ("mailbox:b", ["m2", "m3"])] {
        let set = cli(&[&["-p", &p, "SADD", owner][..], &members].concat());
        assert_eq!(set.code, 0, "{}", set.stderr);
    }
    let overlap = cli(&["-p", &p, "--kevy", "lint", "overlap", "--prefix", "mailbox:"]);
    assert!(
        overlap.stdout.contains("2 owner(s) under mailbox:, 3 distinct name(s)")
            && overlap.stdout.contains("1 name(s) appear under more than one owner:")
            && overlap.code == 1,
        "{}{}",
        overlap.stdout,
        overlap.stderr
    );
    three_rows(&p);
    let declare = "TABLE.DECLARE u PREFIX u: PK id COLUMN id str COLUMN name str COLUMN alias str";
    assert_eq!(cli(&[&["-p", &p][..], &declare.split(' ').collect::<Vec<_>>()].concat()).code, 0);
    for i in 1..=3 {
        cli(&[
            "-p",
            &p,
            "HSET",
            &format!("u:{i}"),
            "id",
            &i.to_string(),
            "alias",
            &format!("n{i}"),
        ]);
    }
    let columns =
        cli(&["-p", &p, "--kevy", "lint", "columns", "u", "--sample", "10", "--threshold", "50"]);
    assert!(
        columns.stdout.starts_with("u: 3 row(s) sampled under u:")
            && columns.stdout.contains("alias and name agree on 100% (3/3)")
            && columns.code == 0,
        "{}{}",
        columns.stdout,
        columns.stderr
    );
    let agrees = cli(&[
        "-p",
        &p,
        "--kevy",
        "shadow",
        "--old",
        "SMEMBERS mailbox:a",
        "--new",
        "SMEMBERS mailbox:a",
        "--new-flat",
        "--samples",
        "2",
    ]);
    assert!(
        agrees.stdout.contains("shadow: 2 samples, 0 divergences") && agrees.code == 0,
        "{}{}",
        agrees.stdout,
        agrees.stderr
    );
    let diverges = cli(&[
        "-p",
        &p,
        "--kevy",
        "shadow",
        "--old",
        "SMEMBERS mailbox:a",
        "--new",
        "SMEMBERS mailbox:b",
        "--new-flat",
    ]);
    assert_eq!(diverges.code, 1, "different membership diverges: {}", diverges.stdout);
    assert!(
        diverges.stdout.contains("m1") || diverges.stdout.contains("m3"),
        "{}",
        diverges.stdout
    );
}

#[test]
fn tool_arguments_are_read_strictly() {
    let s = Srv::start();
    let p = s.port();
    for (args, text) in [
        (
            &["digest", "-p", "1", "u:"][..],
            "kevy-cli digest: -p is a connection option; give it before --kevy",
        ),
        (
            &["copy-prefix", "--rate", "x", "u:", "v:"][..],
            "kevy-cli copy-prefix: --rate takes a number, not 'x'",
        ),
        (
            &["shadow", "--old", "GET a", "--new", "GET b", "--samples", "many"][..],
            "kevy-cli shadow: --samples takes a number, not 'many'",
        ),
        (&["doctor", "--bogus"][..], "kevy-cli doctor: unexpected '--bogus'"),
        (&["lint", "columns"][..], "kevy-cli lint columns: name a declared table"),
        (&["lint", "sideways"][..], "kevy-cli lint: unknown subcommand 'sideways'"),
        (&["backfill-keys"][..], "kevy-cli backfill-keys: give at least one source"),
        (
            &["sql", "compile", "s.sql", "--url", "h:1"][..],
            "kevy-cli sql: --url is a connection option; give it before --kevy",
        ),
        (&["export", "--resume", "f"][..], "kevy-cli export: unexpected '--resume'"),
        (&["diff", "nowhere", "u:"][..], "kevy-cli diff: 'nowhere' is not host:port"),
        (&["restore", "--from", "x"][..], "kevy-cli restore: --to missing"),
        (&["backup", "--to", "x"][..], "kevy-cli backup: --data-dir missing"),
        (&["backup", "--data-dir"][..], "kevy-cli backup: --data-dir needs a value"),
        (&["diff", "127.0.0.1:1"][..], "kevy-cli diff: wrong arguments"),
        (&["sql", "nonsense", "f.sql"][..], "kevy-cli sql: unknown sql subcommand 'nonsense'"),
        (&["sql", "probe"][..], "kevy-cli sql: probe takes one corpus directory"),
        (&["import"][..], "kevy-cli import: give the file"),
    ] {
        let out = cli(&[&["-p", &p, "--kevy"][..], args].concat());
        assert!(out.stderr.starts_with(text) && out.code == 1, "{args:?}: {}", out.stderr);
    }
}

#[test]
fn bare_shipped_words_keep_working_with_a_deprecation_line() {
    let (a, b) = (Srv::start(), Srv::start());
    let (pa, pb) = (a.port(), b.port());
    three_rows(&pa);
    const WARNING: &str = "kevy-cli: `kevy-cli digest ...` is deprecated and goes away in 7.0; use `kevy-cli [-h host] [-p port] --kevy digest ...`\n";
    let digest = cli(&["digest", "-p", &pa, "u:"]);
    assert!(digest.stdout.starts_with("3 keys ") && digest.stderr == WARNING, "{}", digest.stderr);
    let bad = cli(&["digest", "-p", "abc", "u:"]);
    assert_eq!(
        (bad.stderr.strip_prefix(WARNING), bad.code),
        (Some("kevy-cli digest: -p takes a port, not 'abc'\n"), 1)
    );
    let (ea, eb) = (format!("127.0.0.1:{pa}"), format!("127.0.0.1:{pb}"));
    assert!(cli(&["diff", &ea, &eb, "u:"]).stdout.contains("MISMATCH"));
    let dir = scratch("bare");
    let schema = dir.join("s.sql");
    std::fs::write(&schema, "CREATE TABLE t (id bigint PRIMARY KEY);\n").unwrap();
    let applied = cli(&["sql", "compile", schema.to_str().unwrap(), "--apply", "--url", &ea]);
    assert_eq!(applied.stdout, "TABLE.DECLARE t → OK\n", "{}", applied.stderr);
    let _ = std::fs::remove_dir_all(&dir);
}

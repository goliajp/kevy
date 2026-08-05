//! `kevy-cli sql plan <file.sql>` — the exit-code contract.
//!
//! The counts and the refusal texts are kevy-sql's, tested there. What
//! belongs here is what a migration script actually consumes: whether
//! this schema can move as it stands. No server: `plan` reads a file
//! and reports, which is the point of it being a separate subcommand
//! from `compile --apply`.

use std::process::Command;

const SERVED: &str = r"
CREATE TABLE orders (
  id      bigserial PRIMARY KEY,
  status  text
);
CREATE INDEX ON orders (status);
CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';
";

const ONE_UNSERVED: &str = r"
CREATE TABLE orders (
  id      bigserial PRIMARY KEY,
  status  text,
  total   bigint
);
CREATE INDEX ON orders (status);
CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';
CREATE VIEW by_total AS SELECT * FROM orders WHERE total = $1;
";

/// One directory per call: these run in parallel threads of one
/// process, so a pid-only name is the *same* name and the tests
/// overwrite each other's schema.
static NTH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn plan(sql: &str) -> (bool, String) {
    let nth = NTH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("kevy-plan-{}-{nth}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("schema.sql");
    std::fs::write(&file, sql).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_kevy-cli"))
        .args(["sql", "plan"])
        .arg(&file)
        .output()
        .expect("run kevy-cli");
    let _ = std::fs::remove_dir_all(&dir);
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

#[test]
fn a_schema_whose_queries_are_all_served_exits_zero() {
    let (ok, text) = plan(SERVED);
    assert!(ok, "should exit 0:\n{text}");
    assert!(text.contains("every query is served"), "{text}");
}

/// An unserved query is not a warning: that query cannot run at all, so
/// it blocks the move until the schema changes.
#[test]
fn one_unserved_query_fails_the_plan() {
    let (ok, text) = plan(ONE_UNSERVED);
    assert!(!ok, "should exit non-zero:\n{text}");
    assert!(text.contains("paid"), "the served one is still listed:\n{text}");
    assert!(text.contains("CREATE INDEX ON orders (total)"), "names the fix:\n{text}");
}

/// The other half of the deliberate line: a schema that does not parse
/// has no plan, and that stays an error with its position.
#[test]
fn a_schema_that_does_not_parse_is_an_error_with_a_position() {
    let (ok, text) = plan("CREATE TABLE orders (id bigserial PRIMARY KEY,;");
    assert!(!ok);
    assert!(text.contains("line 1, col"), "{text}");
}

/// `plan` reads and reports; there is nothing for `--apply` to mean.
#[test]
fn plan_refuses_apply_rather_than_ignoring_it() {
    let out = Command::new(env!("CARGO_BIN_EXE_kevy-cli"))
        .args(["sql", "plan", "whatever.sql", "--apply"])
        .output()
        .expect("run kevy-cli");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("never applies anything"), "{err}");
}

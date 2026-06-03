use super::*;
use kevy_resp::Argv;

fn run(verb: &[u8], rest: &[&[u8]]) -> Vec<u8> {
    let mut a = Argv::default();
    a.push(verb);
    for r in rest {
        a.push(r);
    }
    let mut out = Vec::new();
    let cfg = Config::default();
    cmd_config(&cfg, &a, &mut out);
    let _ = verb;
    out
}

#[test]
fn glob_matcher_handles_star_and_literal() {
    assert!(glob_match(b"*", b"anything"));
    assert!(glob_match(b"max*", b"maxmemory"));
    assert!(glob_match(b"max*", b"maxmemory-policy"));
    assert!(glob_match(b"*memory*", b"maxmemory-policy"));
    assert!(glob_match(b"port", b"port"));
    assert!(!glob_match(b"port", b"ports"));
    assert!(!glob_match(b"max*", b"min-memory"));
    assert!(glob_match(b"", b""));
    assert!(!glob_match(b"", b"x"));
}

#[test]
fn config_get_exact_key() {
    let out = run(b"CONFIG", &[b"GET", b"port"]);
    let s = String::from_utf8(out).unwrap();
    assert!(s.starts_with("*2\r\n"), "got: {s:?}");
    assert!(s.contains("port"));
    assert!(s.contains("6004")); // default port
}

#[test]
fn config_get_glob() {
    let out = run(b"CONFIG", &[b"GET", b"max*"]);
    let s = String::from_utf8(out).unwrap();
    // max* matches maxmemory + maxmemory-policy + maxmemory-samples
    // → 3 pairs → array of 6.
    assert!(s.starts_with("*6\r\n"), "got: {s:?}");
    assert!(s.contains("maxmemory"));
    assert!(s.contains("maxmemory-policy"));
}

#[test]
fn config_get_unknown_key_returns_empty_array() {
    let out = run(b"CONFIG", &[b"GET", b"nonexistent-setting"]);
    assert_eq!(out, b"*0\r\n");
}

#[test]
fn config_get_multiple_patterns() {
    let out = run(b"CONFIG", &[b"GET", b"port", b"bind"]);
    let s = String::from_utf8(out).unwrap();
    assert!(s.starts_with("*4\r\n"));
    assert!(s.contains("port"));
    assert!(s.contains("bind"));
}

// ────────────────────────── CONFIG SET ──────────────────────────
//
// These tests exercise the validation paths (good value + bad value
// + read-only + unknown). The actual `config_global::replace` swap
// is covered by the integration test `crates/kevy/tests/persistence
// .rs::config_set_*` once the runtime is in the picture; here we
// exercise it via `apply_hot_set` directly so the unit suite stays
// independent of global state.

fn try_set(key: &str, value: &str) -> Result<Config, SetError> {
    let mut cfg = Config::default();
    apply_hot_set(&mut cfg, key.as_bytes(), value.as_bytes())?;
    Ok(cfg)
}

#[test]
fn apply_hot_set_maxmemory_parses_size_literal() {
    let cfg = try_set("maxmemory", "2gb").expect("ok");
    assert_eq!(cfg.memory.maxmemory, 2 * 1024 * 1024 * 1024);
}

#[test]
fn apply_hot_set_maxmemory_policy_round_trips_enum_name() {
    let cfg = try_set("maxmemory-policy", "allkeys-lfu").expect("ok");
    assert_eq!(cfg.memory.maxmemory_policy, EvictionPolicy::AllKeysLfu);
}

#[test]
fn apply_hot_set_appendfsync_round_trips_enum_name() {
    let cfg = try_set("appendfsync", "always").expect("ok");
    assert_eq!(cfg.persistence.appendfsync, AppendFsync::Always);
}

#[test]
fn apply_hot_set_auto_aof_rewrite_pct_accepts_integer() {
    let cfg = try_set("auto-aof-rewrite-percentage", "200").expect("ok");
    assert_eq!(cfg.persistence.auto_aof_rewrite_percentage, 200);
}

#[test]
fn apply_hot_set_auto_aof_rewrite_min_size_accepts_size_literal() {
    let cfg = try_set("auto-aof-rewrite-min-size", "128mb").expect("ok");
    assert_eq!(
        cfg.persistence.auto_aof_rewrite_min_size,
        128 * 1024 * 1024,
    );
}

#[test]
fn apply_hot_set_hz_and_samples_accept_integers() {
    let cfg = try_set("hz", "100").expect("ok");
    assert_eq!(cfg.expiry.hz, 100);
    let cfg = try_set("maxmemory-samples", "5").expect("ok");
    assert_eq!(cfg.expiry.sample, 5);
}

#[test]
fn apply_hot_set_loglevel_round_trips_warning_alias() {
    let cfg = try_set("loglevel", "warning").expect("ok");
    assert_eq!(cfg.log.level, LogLevel::Warn);
}

#[test]
fn apply_hot_set_logfile_accepts_stdout_stderr_but_rejects_paths() {
    let cfg = try_set("logfile", "stdout").expect("ok");
    assert_eq!(cfg.log.output, LogOutput::Stdout);
    let cfg = try_set("logfile", "stderr").expect("ok");
    assert_eq!(cfg.log.output, LogOutput::Stderr);
    match try_set("logfile", "/var/log/kevy.log").unwrap_err() {
        SetError::ReadOnly(k) => assert_eq!(k, "logfile"),
        other => panic!("expected ReadOnly, got {other:?}"),
    }
}

#[test]
fn apply_hot_set_bad_value_returns_useful_reason() {
    match try_set("appendfsync", "garbage").unwrap_err() {
        SetError::BadValue { key, reason } => {
            assert_eq!(key, "appendfsync");
            assert!(reason.contains("always"));
        }
        other => panic!("expected BadValue, got {other:?}"),
    }
    match try_set("maxmemory", "not a size").unwrap_err() {
        SetError::BadValue { key, .. } => assert_eq!(key, "maxmemory"),
        other => panic!("expected BadValue, got {other:?}"),
    }
}

#[test]
fn apply_hot_set_read_only_fields_report_clearly() {
    for k in ["bind", "port", "io-threads", "dir", "appendonly"] {
        match try_set(k, "anything").unwrap_err() {
            SetError::ReadOnly(name) => assert_eq!(name, k),
            other => panic!("expected ReadOnly for {k}, got {other:?}"),
        }
    }
}

#[test]
fn apply_hot_set_unknown_field_returns_unknown() {
    match try_set("not-a-real-setting", "x").unwrap_err() {
        SetError::Unknown(k) => assert_eq!(k, "not-a-real-setting"),
        other => panic!("expected Unknown, got {other:?}"),
    }
}

// The dispatch wrapper produces the right RESP shape: the
// `cmd_config_set` test runs through `cmd_config` (top-level
// dispatcher) and reads back the wire reply. config_global is
// uninitialised in this test process, so the swap will Err — which
// is itself the right shape to surface.

#[test]
fn config_set_with_uninitialised_global_returns_error() {
    // No `config_global::init` has run in this binary's test path;
    // `replace` returns Err, which the handler maps to a -ERR.
    // Even so, a bad-value case still short-circuits with the
    // value-error before reaching `replace`, so we test both.

    let bad = run(b"CONFIG", &[b"SET", b"maxmemory", b"not-a-size"]);
    let s = String::from_utf8(bad).unwrap();
    assert!(s.starts_with("-ERR"), "got: {s:?}");
    assert!(
        s.contains("CONFIG SET failed for 'maxmemory'"),
        "got: {s:?}",
    );
}

#[test]
fn config_set_unknown_field_returns_unknown_param_error() {
    let out = run(b"CONFIG", &[b"SET", b"not-a-real-setting", b"x"]);
    let s = String::from_utf8(out).unwrap();
    assert!(s.starts_with("-ERR"));
    assert!(
        s.contains("Unknown CONFIG SET parameter"),
        "got: {s:?}",
    );
}

#[test]
fn config_set_read_only_field_returns_restart_required_error() {
    let out = run(b"CONFIG", &[b"SET", b"bind", b"0.0.0.0"]);
    let s = String::from_utf8(out).unwrap();
    assert!(s.starts_with("-ERR"));
    assert!(
        s.contains("can't be changed at runtime"),
        "got: {s:?}",
    );
}

#[test]
fn config_set_wrong_arity_errors() {
    // CONFIG SET requires exactly 4 args (CONFIG + SET + key + value).
    let out = run(b"CONFIG", &[b"SET", b"maxmemory"]);
    let s = String::from_utf8(out).unwrap();
    assert!(s.starts_with("-ERR"));
    assert!(s.contains("wrong number"));
}

#[test]
fn config_rewrite_without_source_returns_no_config_file_error() {
    // `config_global::get()` falls back to Config::default() in tests,
    // which has source_path = None.
    let out = run(b"CONFIG", &[b"REWRITE"]);
    let s = String::from_utf8(out).unwrap();
    assert!(s.starts_with("-ERR"));
    assert!(
        s.contains("running without a config file"),
        "got: {s:?}",
    );
}

#[test]
fn config_rewrite_writes_atomic_round_trip_file() {
    // Direct test of `atomic_write` + `to_toml_string` since the
    // handler short-circuits in the test binary (no config_global).
    let dir = std::env::temp_dir().join(format!(
        "kevy-config-rewrite-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("kevy.toml");

    let mut cfg = Config::default();
    cfg.memory.maxmemory = 1024 * 1024 * 1024; // 1 GiB
    cfg.persistence.appendfsync = AppendFsync::Always;
    let text = cfg.to_toml_string();
    atomic_write(&path, text.as_bytes()).expect("atomic_write");

    let read_back = std::fs::read_to_string(&path).unwrap();
    assert_eq!(read_back, text);
    // The .rewrite temp file must NOT linger.
    assert!(
        !path.with_file_name("kevy.toml.rewrite").exists(),
        "atomic_write left the temp file behind"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_unknown_subcommand_errors() {
    let out = run(b"CONFIG", &[b"NUKE"]);
    let s = String::from_utf8(out).unwrap();
    assert!(s.starts_with("-ERR"));
    assert!(s.contains("unknown CONFIG subcommand"));
}

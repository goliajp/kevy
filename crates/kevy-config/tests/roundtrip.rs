//! End-to-end integration tests: TOML source → `Config` value → schema
//! field check. Covers the full lex + parse + apply path together to
//! catch any per-module assumption that drifts.

use kevy_config::{
    AppendFsync, CliOverrides, Config, ConfigError, EvictionPolicy, LogLevel, LogOutput,
};
use std::path::PathBuf;

#[test]
fn full_schema_round_trip() {
    let src = r#"
# kevy.toml — every field exercised
[server]
bind     = "0.0.0.0"
port     = 7000
threads  = 4
data_dir = "/var/lib/kevy"

[persistence]
aof                          = true
appendfsync                  = "always"
auto_aof_rewrite_percentage  = 200
auto_aof_rewrite_min_size    = "128mb"

[memory]
maxmemory        = "2gb"
maxmemory_policy = "allkeys-lru"

[expiry]
hz     = 20
sample = 30

[log]
level  = "debug"
output = "stdout"
"#;
    let cfg = Config::from_toml_str(src, None).unwrap();
    assert_eq!(cfg.server.bind, [0, 0, 0, 0]);
    assert_eq!(cfg.server.port, 7000);
    assert_eq!(cfg.server.threads, 4);
    assert_eq!(cfg.server.data_dir, PathBuf::from("/var/lib/kevy"));
    assert!(cfg.persistence.aof);
    assert_eq!(cfg.persistence.appendfsync, AppendFsync::Always);
    assert_eq!(cfg.persistence.auto_aof_rewrite_percentage, 200);
    assert_eq!(cfg.persistence.auto_aof_rewrite_min_size, 128 * 1024 * 1024);
    assert_eq!(cfg.memory.maxmemory, 2 * 1024 * 1024 * 1024);
    assert_eq!(cfg.memory.maxmemory_policy, EvictionPolicy::AllKeysLru);
    assert_eq!(cfg.expiry.hz, 20);
    assert_eq!(cfg.expiry.sample, 30);
    assert_eq!(cfg.log.level, LogLevel::Debug);
    assert_eq!(cfg.log.output, LogOutput::Stdout);
}

#[test]
fn precedence_chain_cli_beats_env_beats_file_beats_default() {
    // Start with the file load (server.port = 7000).
    let mut cfg = Config::from_toml_str("[server]\nport = 7000\n", None).unwrap();
    assert_eq!(cfg.server.port, 7000); // file > default (6004)

    // Env overlay (KEVY_PORT = 7001) > file.
    cfg.merge_env([("KEVY_PORT", "7001")]).unwrap();
    assert_eq!(cfg.server.port, 7001);

    // CLI overlay (port = 7002) > env.
    cfg.merge_cli(CliOverrides {
        port: Some(7002),
        ..CliOverrides::default()
    })
    .unwrap();
    assert_eq!(cfg.server.port, 7002);
}

#[test]
fn empty_toml_yields_defaults() {
    let cfg = Config::from_toml_str("", None).unwrap();
    assert_eq!(cfg, Config::default());
}

#[test]
fn only_comments_yields_defaults() {
    let cfg = Config::from_toml_str("# just a comment\n# another\n", None).unwrap();
    assert_eq!(cfg, Config::default());
}

#[test]
fn unknown_section_errors_with_line() {
    let err = Config::from_toml_str("[bogus]\nx = 1\n", None).unwrap_err();
    match err {
        ConfigError::Schema { line, field, .. } => {
            assert_eq!(line, 2); // assignment line
            assert!(field.contains("bogus"), "field was {field:?}");
        }
        other => panic!("expected Schema, got {other:?}"),
    }
}

#[test]
fn unknown_key_errors_with_line() {
    let err =
        Config::from_toml_str("[server]\nbogus_setting = 1\n", None).unwrap_err();
    assert!(matches!(err, ConfigError::Schema { .. }));
}

#[test]
fn invalid_enum_value_errors() {
    let err = Config::from_toml_str(
        "[memory]\nmaxmemory_policy = \"random-everywhere\"\n",
        None,
    )
    .unwrap_err();
    match err {
        ConfigError::Schema { field, msg, .. } => {
            assert!(field.contains("maxmemory_policy"));
            assert!(msg.contains("must be one of"));
        }
        other => panic!("expected Schema, got {other:?}"),
    }
}

#[test]
fn size_literal_or_bare_int_both_accepted() {
    let cfg = Config::from_toml_str("[memory]\nmaxmemory = 1024\n", None).unwrap();
    assert_eq!(cfg.memory.maxmemory, 1024);
    let cfg2 = Config::from_toml_str("[memory]\nmaxmemory = \"1kb\"\n", None).unwrap();
    assert_eq!(cfg2.memory.maxmemory, 1024);
}

#[test]
fn log_output_file_path_round_trips() {
    let cfg = Config::from_toml_str("[log]\noutput = \"/var/log/kevy.log\"\n", None).unwrap();
    assert_eq!(cfg.log.output, LogOutput::File(PathBuf::from("/var/log/kevy.log")));
}

#[test]
fn cli_no_aof_overrides_file_aof_true() {
    let mut cfg = Config::from_toml_str("[persistence]\naof = true\n", None).unwrap();
    cfg.merge_cli(CliOverrides {
        aof: Some(false),
        ..CliOverrides::default()
    })
    .unwrap();
    assert!(!cfg.persistence.aof);
}

#[test]
fn source_path_recorded_when_provided() {
    let p = std::path::Path::new("/tmp/fake-kevy.toml");
    let cfg = Config::from_toml_str("", Some(p)).unwrap();
    assert_eq!(cfg.source_path.as_deref(), Some(p));
}

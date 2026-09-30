//! Every section key refuses a value of the wrong shape, naming the field
//! and the line it sat on.

use kevy_config::{Config, ConfigError, LogOutput};

/// Load `[section]\n<line>` and return the refusal's field and message;
/// the offending line is always line 2.
fn refusal(section: &str, line: &str) -> (String, String) {
    let text = format!("[{section}]\n{line}\n");
    match Config::from_toml_str(&text, None) {
        Err(ConfigError::Schema { line: 2, field, msg }) => (field, msg),
        other => panic!("{text:?} should be refused at line 2, got {other:?}"),
    }
}

#[test]
fn advanced_keys_refuse_values_of_the_wrong_type() {
    for key in
        ["spin_limit", "park_timeout_ms", "tick_check_every", "ring_capacity", "recv_buffers"]
    {
        let (field, msg) = refusal("advanced", &format!("{key} = \"many\""));
        assert_eq!(field, format!("[advanced].{key}"));
        assert!(msg.starts_with("expected integer"), "{key}: {msg}");
    }
}

#[test]
fn recv_buffers_must_be_a_power_of_two_within_bounds() {
    for bad in ["3", "65536"] {
        let (_, msg) = refusal("advanced", &format!("recv_buffers = {bad}"));
        assert_eq!(msg, "recv_buffers must be a power of two, 1 to 32768");
    }
    let cfg = Config::from_toml_str("[advanced]\nrecv_buffers = 1024\n", None).unwrap();
    assert_eq!(cfg.advanced.recv_buffers, 1024);
}

#[test]
fn an_unknown_key_is_refused_by_name_in_advanced_and_cluster() {
    assert_eq!(refusal("advanced", "spin = 1").1, "unknown [advanced] key: spin");
    assert_eq!(refusal("cluster", "leader = 1").1, "unknown [cluster] key: leader");
}

#[test]
fn cluster_keys_refuse_values_of_the_wrong_type() {
    let cases = [
        ("enabled = 1", "expected boolean"),
        ("port_base = true", "expected integer"),
        ("node_id = 7", "expected string"),
        ("elect_port_base = \"x\"", "expected integer"),
        ("peers = 5", "expected a list"),
        ("scopes = true", "expected a list"),
    ];
    for (line, want) in cases {
        let (_, msg) = refusal("cluster", line);
        assert!(msg.starts_with(want), "{line}: {msg}");
    }
}

#[test]
fn a_malformed_peer_or_scope_token_is_quoted_back() {
    let (field, msg) = refusal("cluster", "peers = [\"n1@h:6204\", \"nohost\"]");
    assert_eq!((field.as_str(), msg.as_str()), ("[cluster].peers", "bad peer token: \"nohost\""));
    let (field, msg) = refusal("cluster", "scopes = \"app:=n1,noequals\"");
    assert_eq!(
        (field.as_str(), msg.as_str()),
        ("[cluster].scopes", "bad scope token: \"noequals\"")
    );
}

#[test]
fn a_size_refuses_a_negative_count_a_bad_literal_and_a_non_size() {
    assert_eq!(refusal("memory", "maxmemory = -1").1, "size value -1 must be non-negative");
    assert_eq!(
        refusal("memory", "maxmemory = \"64zb\"").1,
        "size literal \"64zb\" has unknown unit: \"zb\""
    );
    assert_eq!(refusal("memory", "maxmemory = true").1, "expected size literal, got Bool(true)");
    assert!(
        refusal("memory", "maxmemory = [\"1mb\"]").1.starts_with("expected size literal, got Arr(")
    );
}

#[test]
fn a_standard_output_log_names_itself_and_reloads() {
    assert_eq!(LogOutput::Stdout.to_config_str(), "stdout");
    let cfg = Config::from_toml_str("[log]\noutput = \"stdout\"\n", None).unwrap();
    assert_eq!(cfg.log.output, LogOutput::Stdout);
    let again = Config::from_toml_str(&cfg.to_toml_string(), None).unwrap();
    assert_eq!(again.log.output, LogOutput::Stdout);
}

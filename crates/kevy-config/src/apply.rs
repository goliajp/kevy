//! Schema-to-parser glue: `Config::apply_item` (dispatch by section/key)
//! and `Config::apply_env_var`, plus the small value-coercion helpers
//! they need. Kept separate from [`crate::schema`] so the schema file
//! is just "what fields exist" and this file is "how to set them".

use std::path::PathBuf;

use crate::parse::{Item, Value};
use crate::schema::{
    AppendFsync, Config, ConfigError, EvictionPolicy, LogLevel, LogOutput,
};
use crate::size::parse_size;

impl Config {
    /// Apply a single parsed item (one `(section, key, value)` triple).
    pub(crate) fn apply_item(&mut self, item: Item) -> Result<(), ConfigError> {
        let section = item.section.as_deref().unwrap_or("");
        match section {
            "server" => self.apply_server(item),
            "persistence" => self.apply_persistence(item),
            "memory" => self.apply_memory(item),
            "expiry" => self.apply_expiry(item),
            "log" => self.apply_log(item),
            "notification" => self.apply_notification(item),
            "advanced" => self.apply_advanced(item),
            "slowlog" => self.apply_slowlog(item),
            "cluster" => self.apply_cluster(item),
            other => Err(schema_err(&item, format!("unknown section [{other}]"))),
        }
    }

    fn apply_server(&mut self, item: Item) -> Result<(), ConfigError> {
        match item.key.as_str() {
            "bind" => {
                self.server.bind = parse_ipv4(&value_as_string(&item)?).ok_or_else(|| {
                    schema_err(&item, "bind must be a dotted-quad IPv4 string")
                })?;
            }
            "port" => self.server.port = value_as_u16(&item)?,
            "threads" => self.server.threads = value_as_usize(&item)?,
            "data_dir" => self.server.data_dir = PathBuf::from(value_as_string(&item)?),
            k => return Err(schema_err(&item, format!("unknown [server] key: {k}"))),
        }
        Ok(())
    }

    fn apply_persistence(&mut self, item: Item) -> Result<(), ConfigError> {
        match item.key.as_str() {
            "aof" => self.persistence.aof = value_as_bool(&item)?,
            "appendfsync" => {
                self.persistence.appendfsync = parse_appendfsync(&value_as_string(&item)?)
                    .ok_or_else(|| {
                        schema_err(
                            &item,
                            "appendfsync must be 'always' | 'everysec' | 'no'",
                        )
                    })?;
            }
            "auto_aof_rewrite_percentage" => {
                self.persistence.auto_aof_rewrite_percentage = value_as_u32(&item)?;
            }
            "auto_aof_rewrite_min_size" => {
                self.persistence.auto_aof_rewrite_min_size = value_as_size(&item)?;
            }
            k => return Err(schema_err(&item, format!("unknown [persistence] key: {k}"))),
        }
        Ok(())
    }

    fn apply_memory(&mut self, item: Item) -> Result<(), ConfigError> {
        match item.key.as_str() {
            "maxmemory" => self.memory.maxmemory = value_as_size(&item)?,
            "maxmemory_policy" => {
                self.memory.maxmemory_policy = parse_eviction(&value_as_string(&item)?)
                    .ok_or_else(|| {
                        schema_err(
                            &item,
                            "maxmemory_policy must be one of: noeviction, allkeys-lru, \
                             allkeys-lfu, allkeys-random, volatile-lru, volatile-lfu, \
                             volatile-random, volatile-ttl",
                        )
                    })?;
            }
            k => return Err(schema_err(&item, format!("unknown [memory] key: {k}"))),
        }
        Ok(())
    }

    fn apply_expiry(&mut self, item: Item) -> Result<(), ConfigError> {
        match item.key.as_str() {
            "hz" => self.expiry.hz = value_as_u32(&item)?,
            "sample" => self.expiry.sample = value_as_u32(&item)?,
            k => return Err(schema_err(&item, format!("unknown [expiry] key: {k}"))),
        }
        Ok(())
    }

    fn apply_log(&mut self, item: Item) -> Result<(), ConfigError> {
        match item.key.as_str() {
            "level" => {
                self.log.level = parse_log_level(&value_as_string(&item)?).ok_or_else(|| {
                    schema_err(
                        &item,
                        "log.level must be 'trace' | 'debug' | 'info' | 'warn' | 'error'",
                    )
                })?;
            }
            "output" => self.log.output = parse_log_output(&value_as_string(&item)?),
            k => return Err(schema_err(&item, format!("unknown [log] key: {k}"))),
        }
        Ok(())
    }

    fn apply_notification(&mut self, item: Item) -> Result<(), ConfigError> {
        match item.key.as_str() {
            "notify_keyspace_events" => {
                self.notification.notify_keyspace_events = value_as_string(&item)?;
            }
            k => return Err(schema_err(&item, format!("unknown [notification] key: {k}"))),
        }
        Ok(())
    }

    fn apply_advanced(&mut self, item: Item) -> Result<(), ConfigError> {
        match item.key.as_str() {
            "spin_limit" => self.advanced.spin_limit = value_as_u32(&item)?,
            "park_timeout_ms" => self.advanced.park_timeout_ms = value_as_u32(&item)?,
            "tick_check_every" => self.advanced.tick_check_every = value_as_u32(&item)?,
            "ring_capacity" => self.advanced.ring_capacity = value_as_usize(&item)?,
            k => return Err(schema_err(&item, format!("unknown [advanced] key: {k}"))),
        }
        Ok(())
    }

    fn apply_slowlog(&mut self, item: Item) -> Result<(), ConfigError> {
        match item.key.as_str() {
            "slower_than_micros" => self.slowlog.slower_than_micros = value_as_i64(&item)?,
            "max_len" => self.slowlog.max_len = value_as_u32(&item)?,
            k => return Err(schema_err(&item, format!("unknown [slowlog] key: {k}"))),
        }
        Ok(())
    }

    fn apply_cluster(&mut self, item: Item) -> Result<(), ConfigError> {
        match item.key.as_str() {
            "enabled" => self.cluster.enabled = value_as_bool(&item)?,
            "port_base" => self.cluster.port_base = value_as_u16(&item)?,
            k => return Err(schema_err(&item, format!("unknown [cluster] key: {k}"))),
        }
        Ok(())
    }

    /// Apply one env var. Recognised names (others ignored):
    /// `KEVY_BIND`, `KEVY_PORT`, `KEVY_THREADS`, `KEVY_DIR`, `KEVY_AOF`,
    /// `KEVY_CLUSTER`.
    pub(crate) fn apply_env_var(
        &mut self,
        name: &str,
        value: &str,
    ) -> Result<(), ConfigError> {
        match name {
            "KEVY_BIND" => {
                self.server.bind = parse_ipv4(value).ok_or_else(|| ConfigError::Schema {
                    line: 0,
                    field: "[env] KEVY_BIND".into(),
                    msg: "must be a dotted-quad IPv4".into(),
                })?;
            }
            "KEVY_PORT" => {
                self.server.port = value.parse().map_err(|_| ConfigError::Schema {
                    line: 0,
                    field: "[env] KEVY_PORT".into(),
                    msg: format!("must be 0..=65535, got {value:?}"),
                })?;
            }
            "KEVY_THREADS" => {
                self.server.threads = value.parse().map_err(|_| ConfigError::Schema {
                    line: 0,
                    field: "[env] KEVY_THREADS".into(),
                    msg: format!("must be a non-negative integer, got {value:?}"),
                })?;
            }
            "KEVY_DIR" => self.server.data_dir = PathBuf::from(value),
            "KEVY_AOF" => {
                self.persistence.aof = !matches!(value, "0" | "off" | "false" | "no");
            }
            "KEVY_CLUSTER" => {
                self.cluster.enabled = !matches!(value, "0" | "off" | "false" | "no");
            }
            _ => {}
        }
        Ok(())
    }
}

// ───────────── value coercion helpers ─────────────

fn value_as_string(item: &Item) -> Result<String, ConfigError> {
    match &item.value {
        Value::Str(s) => Ok(s.clone()),
        other => Err(schema_err(item, format!("expected string, got {other:?}"))),
    }
}

fn value_as_bool(item: &Item) -> Result<bool, ConfigError> {
    match item.value {
        Value::Bool(b) => Ok(b),
        ref other => Err(schema_err(item, format!("expected boolean, got {other:?}"))),
    }
}

fn value_as_u16(item: &Item) -> Result<u16, ConfigError> {
    let n = value_as_i64(item)?;
    u16::try_from(n).map_err(|_| schema_err(item, format!("value {n} out of range for u16")))
}

fn value_as_u32(item: &Item) -> Result<u32, ConfigError> {
    let n = value_as_i64(item)?;
    u32::try_from(n).map_err(|_| schema_err(item, format!("value {n} out of range for u32")))
}

fn value_as_usize(item: &Item) -> Result<usize, ConfigError> {
    let n = value_as_i64(item)?;
    usize::try_from(n).map_err(|_| schema_err(item, format!("value {n} out of range for usize")))
}

fn value_as_i64(item: &Item) -> Result<i64, ConfigError> {
    match item.value {
        Value::Int(n) => Ok(n),
        ref other => Err(schema_err(item, format!("expected integer, got {other:?}"))),
    }
}

/// Accept either an integer (bytes) or a size literal string ("64mb").
fn value_as_size(item: &Item) -> Result<u64, ConfigError> {
    match &item.value {
        Value::Int(n) => u64::try_from(*n)
            .map_err(|_| schema_err(item, format!("size value {n} must be non-negative"))),
        Value::Str(s) => parse_size(s).map_err(|e| schema_err(item, e)),
        other @ Value::Bool(_) => {
            Err(schema_err(item, format!("expected size literal, got {other:?}")))
        }
    }
}

fn schema_err(item: &Item, msg: impl Into<String>) -> ConfigError {
    let field = match &item.section {
        Some(s) => format!("[{}].{}", s, item.key),
        None => item.key.clone(),
    };
    ConfigError::Schema {
        line: item.line,
        field,
        msg: msg.into(),
    }
}

// ───────────── small value parsers ─────────────

pub(crate) fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut parts = s.split('.');
    for slot in &mut octets {
        *slot = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(octets)
}

// Enum (string <-> variant) helpers moved to schema.rs as inherent
// methods so `CONFIG SET` / `CONFIG REWRITE` share the same canonical
// names as the TOML parser. These thin wrappers preserve the
// pre-existing call sites.

fn parse_appendfsync(s: &str) -> Option<AppendFsync> {
    AppendFsync::parse(s)
}

fn parse_eviction(s: &str) -> Option<EvictionPolicy> {
    EvictionPolicy::parse(s)
}

fn parse_log_level(s: &str) -> Option<LogLevel> {
    LogLevel::parse(s)
}

fn parse_log_output(s: &str) -> LogOutput {
    LogOutput::parse(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_parser() {
        assert_eq!(parse_ipv4("127.0.0.1"), Some([127, 0, 0, 1]));
        assert_eq!(parse_ipv4("0.0.0.0"), Some([0, 0, 0, 0]));
        assert_eq!(parse_ipv4("192.168.1.255"), Some([192, 168, 1, 255]));
        assert_eq!(parse_ipv4("256.0.0.1"), None); // octet overflow
        assert_eq!(parse_ipv4("1.2.3"), None); // too few
        assert_eq!(parse_ipv4("1.2.3.4.5"), None); // too many
    }

    #[test]
    fn enum_parsers_are_case_insensitive() {
        assert_eq!(parse_appendfsync("EVERYSEC"), Some(AppendFsync::EverySec));
        assert_eq!(parse_appendfsync("Always"), Some(AppendFsync::Always));
        assert_eq!(parse_appendfsync("garbage"), None);

        assert_eq!(parse_eviction("ALLKEYS-LRU"), Some(EvictionPolicy::AllKeysLru));
        assert_eq!(parse_eviction("Volatile-Ttl"), Some(EvictionPolicy::VolatileTtl));
        assert_eq!(parse_eviction("garbage"), None);

        assert_eq!(parse_log_level("INFO"), Some(LogLevel::Info));
        assert_eq!(parse_log_level("warning"), Some(LogLevel::Warn));
        assert_eq!(parse_log_level("garbage"), None);
    }
}

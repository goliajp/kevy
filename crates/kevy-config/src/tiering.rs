//! `[tiering]` section: the transparent-tiering RAM budget in its
//! three forms —
//! `"auto"` (0.70 × the detected memory bound), `"70%"` (percent of the
//! bound), `"4gb"` / plain bytes — plus the optional spill-dir override.
//!
//! Resolution to bytes is pure math here ([`TierBudgetSpec::resolve_with`]);
//! the OS probe itself lives in `kevy-sys::detected_memory_bound` (the
//! sanctioned boundary) and its result is passed in, keeping this crate
//! 0-dep. The budget is re-resolved on the shard tick (the CONFIG-SET
//! reapply precedent) so cgroup limit changes are honored live.

use std::path::PathBuf;

use crate::apply::{schema_err, value_as_string};
use crate::parse::{Item, Value};
use crate::schema::{Config, ConfigError};
use crate::size::parse_size;

/// The `auto` form's fraction of the detected bound (RFC §7: 0.70).
const AUTO_PCT: u64 = 70;

/// One tiering budget, as configured (not yet resolved to bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierBudgetSpec {
    /// `"auto"` — 0.70 × the detected memory bound, re-probed on the
    /// shard tick.
    Auto,
    /// `"70%"` — that percent of the detected bound (1..=100).
    Percent(u8),
    /// `"4gb"` / plain integer — absolute bytes.
    Bytes(u64),
}

impl TierBudgetSpec {
    /// Parse the wire/TOML/env text form. Accepts `auto`, `N%`
    /// (1..=100), and any [`parse_size`] literal (`4gb`, `512mb`, bare
    /// bytes). Garbage errors by name.
    pub fn parse(s: &str) -> Result<Self, String> {
        let t = s.trim();
        if t.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        if let Some(pct) = t.strip_suffix('%') {
            let p: u64 = pct
                .trim()
                .parse()
                .map_err(|_| format!("tiering budget percent {s:?} is not a number"))?;
            if p == 0 || p > 100 {
                return Err(format!("tiering budget percent {s:?} must be 1..=100"));
            }
            return Ok(Self::Percent(p as u8));
        }
        parse_size(t)
            .map(Self::Bytes)
            .map_err(|e| format!("tiering budget: {e}"))
    }

    /// Resolve to bytes given the probed memory bound. `Bytes` ignores
    /// the probe; `Auto`/`Percent` return `None` when no bound was
    /// detected (the caller refuses by name — never a silent guess).
    pub fn resolve_with(self, detected_bound: Option<u64>) -> Option<u64> {
        match self {
            Self::Bytes(b) => Some(b),
            Self::Auto => detected_bound.map(|d| d / 100 * AUTO_PCT),
            Self::Percent(p) => detected_bound.map(|d| d / 100 * u64::from(p)),
        }
    }

    /// The canonical config-file text form (`auto` / `70%` / bytes).
    pub fn as_config_string(self) -> String {
        match self {
            Self::Auto => "auto".to_string(),
            Self::Percent(p) => format!("{p}%"),
            Self::Bytes(b) => b.to_string(),
        }
    }
}

/// `[tiering]` section. No `budget` key = tiering OFF (today's paths
/// byte-identical — the A1 gate's precondition).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TieringSection {
    /// The RAM budget. `None` = tiering off.
    pub budget: Option<TierBudgetSpec>,
    /// Cold-tier spill dir override. `None` = `<data_dir>/tier/`.
    /// Setting it implies tiering on (`budget` defaults to `auto`).
    pub spill_dir: Option<PathBuf>,
}

impl Config {
    /// Apply one `[tiering]` item (the `apply_item` dispatch arm).
    pub(crate) fn apply_tiering(&mut self, item: &Item) -> Result<(), ConfigError> {
        match item.key.as_str() {
            "budget" => self.tiering.budget = Some(budget_from_item(item)?),
            "spill_dir" => {
                self.tiering.spill_dir = Some(PathBuf::from(value_as_string(item)?));
                // A spill dir without a budget key means "on, auto".
                self.tiering.budget.get_or_insert(TierBudgetSpec::Auto);
            }
            k => return Err(schema_err(item, format!("unknown [tiering] key: {k}"))),
        }
        Ok(())
    }

    /// The `KEVY_TIER_BUDGET` env arm — all three forms; plain bytes
    /// stay back-compat with the original plain-bytes-only knob.
    pub(crate) fn apply_env_tier_budget(&mut self, value: &str) -> Result<(), ConfigError> {
        self.tiering.budget =
            Some(TierBudgetSpec::parse(value).map_err(|msg| ConfigError::Schema {
                line: 0,
                field: "[env] KEVY_TIER_BUDGET".into(),
                msg,
            })?);
        Ok(())
    }
}

/// Budget coercion for `[tiering] budget`: a string in any of the three
/// forms (`"auto"` / `"70%"` / `"4gb"`), or a bare integer (bytes).
fn budget_from_item(item: &Item) -> Result<TierBudgetSpec, ConfigError> {
    match &item.value {
        Value::Int(n) => u64::try_from(*n)
            .map(TierBudgetSpec::Bytes)
            .map_err(|_| schema_err(item, format!("tiering budget {n} must be non-negative"))),
        Value::Str(s) => TierBudgetSpec::parse(s).map_err(|e| schema_err(item, e)),
        other @ (Value::Bool(_) | Value::Arr(_)) => Err(schema_err(
            item,
            format!("expected \"auto\" | \"70%\" | \"4gb\" | bytes, got {other:?}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_three_forms() {
        assert_eq!(TierBudgetSpec::parse("auto").unwrap(), TierBudgetSpec::Auto);
        assert_eq!(TierBudgetSpec::parse("AUTO").unwrap(), TierBudgetSpec::Auto);
        assert_eq!(TierBudgetSpec::parse("70%").unwrap(), TierBudgetSpec::Percent(70));
        assert_eq!(TierBudgetSpec::parse(" 5 %").unwrap(), TierBudgetSpec::Percent(5));
        assert_eq!(
            TierBudgetSpec::parse("4gb").unwrap(),
            TierBudgetSpec::Bytes(4 * 1024 * 1024 * 1024)
        );
        assert_eq!(TierBudgetSpec::parse("1048576").unwrap(), TierBudgetSpec::Bytes(1 << 20));
    }

    #[test]
    fn garbage_rejected_by_name() {
        for bad in ["", "yes", "0%", "101%", "x%", "12qb"] {
            let e = TierBudgetSpec::parse(bad).unwrap_err();
            assert!(e.contains("tiering budget"), "{bad:?}: {e}");
        }
    }

    #[test]
    fn resolution_math() {
        assert_eq!(TierBudgetSpec::Bytes(42).resolve_with(None), Some(42));
        assert_eq!(TierBudgetSpec::Auto.resolve_with(Some(10_000_000_000)), Some(7_000_000_000));
        assert_eq!(TierBudgetSpec::Percent(50).resolve_with(Some(8_000_000_000)), Some(4_000_000_000));
        assert_eq!(TierBudgetSpec::Auto.resolve_with(None), None);
        assert_eq!(TierBudgetSpec::Percent(30).resolve_with(None), None);
    }

    #[test]
    fn toml_env_cli_parse_all_three_forms() {
        for (text, want) in [
            ("[tiering]\nbudget = \"auto\"\n", TierBudgetSpec::Auto),
            ("[tiering]\nbudget = \"70%\"\n", TierBudgetSpec::Percent(70)),
            ("[tiering]\nbudget = \"4gb\"\n", TierBudgetSpec::Bytes(4 << 30)),
            ("[tiering]\nbudget = 1048576\n", TierBudgetSpec::Bytes(1 << 20)),
        ] {
            let cfg = Config::from_toml_str(text, None).expect(text);
            assert_eq!(cfg.tiering.budget, Some(want), "{text}");
        }
        // Env: the KEVY_TIER_BUDGET back-compat plain-bytes form + the new ones.
        for (val, want) in [
            ("134217728", TierBudgetSpec::Bytes(128 << 20)),
            ("auto", TierBudgetSpec::Auto),
            ("35%", TierBudgetSpec::Percent(35)),
        ] {
            let mut cfg = Config::default();
            cfg.merge_env([("KEVY_TIER_BUDGET", val)]).expect(val);
            assert_eq!(cfg.tiering.budget, Some(want), "{val}");
        }
        // CLI override wins.
        let mut cfg = Config::default();
        cfg.merge_cli(crate::CliOverrides {
            tiering_budget: Some(TierBudgetSpec::Percent(25)),
            ..crate::CliOverrides::default()
        })
        .unwrap();
        assert_eq!(cfg.tiering.budget, Some(TierBudgetSpec::Percent(25)));
    }

    #[test]
    fn toml_garbage_budget_rejected_by_name() {
        let e = Config::from_toml_str("[tiering]\nbudget = \"lots\"\n", None).unwrap_err();
        assert!(format!("{e}").contains("tiering budget"), "{e}");
        let mut cfg = Config::default();
        let e = cfg.merge_env([("KEVY_TIER_BUDGET", "0%")]).unwrap_err();
        assert!(format!("{e}").contains("1..=100"), "{e}");
    }

    #[test]
    fn spill_dir_alone_implies_auto_budget() {
        let cfg =
            Config::from_toml_str("[tiering]\nspill_dir = \"/fast/nvme/tier\"\n", None).unwrap();
        assert_eq!(cfg.tiering.budget, Some(TierBudgetSpec::Auto));
        assert_eq!(cfg.tiering.spill_dir.as_deref(), Some(std::path::Path::new("/fast/nvme/tier")));
    }

    #[test]
    fn emit_round_trips_the_section_and_its_absence() {
        // Off (default): the emitted TOML must reparse to off.
        let off = Config::default();
        let re = Config::from_toml_str(&off.to_toml_string(), None).unwrap();
        assert_eq!(re.tiering, TieringSection::default());
        // On, all fields: byte round trip through the template.
        let mut on = Config::default();
        on.tiering.budget = Some(TierBudgetSpec::Percent(60));
        on.tiering.spill_dir = Some(PathBuf::from("/mnt/tier"));
        let re = Config::from_toml_str(&on.to_toml_string(), None).unwrap();
        assert_eq!(re.tiering, on.tiering);
        for spec in [TierBudgetSpec::Auto, TierBudgetSpec::Bytes(4 << 30)] {
            let mut c = Config::default();
            c.tiering.budget = Some(spec);
            let re = Config::from_toml_str(&c.to_toml_string(), None).unwrap();
            assert_eq!(re.tiering.budget, Some(spec));
        }
    }

    #[test]
    fn canonical_string_round_trips() {
        for spec in [
            TierBudgetSpec::Auto,
            TierBudgetSpec::Percent(35),
            TierBudgetSpec::Bytes(4 * 1024 * 1024 * 1024),
        ] {
            let text = spec.as_config_string();
            assert_eq!(TierBudgetSpec::parse(&text).unwrap(), spec, "{text}");
        }
    }
}

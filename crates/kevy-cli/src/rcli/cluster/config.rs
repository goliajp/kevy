//! The `--cluster-*` flags, read into what each subcommand uses.

use crate::rcli::cnum::{atof, atoi};
use crate::rcli::opts::Opts;

/// Settings for one cluster-manager run.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Config {
    pub(crate) replicas: i32,
    pub(crate) master_id: Option<Vec<u8>>,
    pub(crate) from: Option<Vec<u8>>,
    pub(crate) to: Option<Vec<u8>>,
    pub(crate) from_user: Option<Vec<u8>>,
    pub(crate) from_pass: Option<Vec<u8>>,
    pub(crate) from_askpass: bool,
    /// Slots to move; 0 asks.
    pub(crate) slots: i32,
    /// MIGRATE timeout; `None` when not given.
    pub(crate) timeout_ms: Option<i32>,
    /// Keys per GETKEYSINSLOT batch.
    pub(crate) pipeline: i32,
    /// Percent a node may be off balance before rebalance moves slots.
    pub(crate) threshold: f64,
    /// `node=weight`, as given.
    pub(crate) weights: Vec<Vec<u8>>,
    pub(crate) yes: bool,
    pub(crate) only_masters: bool,
    pub(crate) only_replicas: bool,
    pub(crate) simulate: bool,
    pub(crate) replace: bool,
    pub(crate) copy: bool,
    pub(crate) replica: bool,
    pub(crate) use_empty_masters: bool,
    pub(crate) search_multiple_owners: bool,
    pub(crate) fix_with_unreachable_masters: bool,
    /// valkey's CLUSTER MIGRATESLOTS instead of per-slot MIGRATE.
    pub(crate) use_atomic_slot_migration: bool,
    /// Colour the log (TERM names an xterm).
    pub(crate) color: bool,
    /// `--verbose`.
    pub(crate) verbose: bool,
}

impl Config {
    // LOC-WAIVER: a table — one arm per flag.
    pub(crate) fn from_opts(o: &Opts) -> Config {
        let mut c = Config {
            replicas: 0,
            master_id: None,
            from: None,
            to: None,
            from_user: None,
            from_pass: None,
            from_askpass: false,
            slots: 0,
            timeout_ms: None,
            pipeline: 10,
            threshold: 2.0,
            weights: Vec::new(),
            yes: std::env::var_os("REDISCLI_CLUSTER_YES").is_some_and(|v| v == "1"),
            only_masters: false,
            only_replicas: false,
            simulate: false,
            replace: false,
            copy: false,
            replica: false,
            use_empty_masters: false,
            search_multiple_owners: false,
            fix_with_unreachable_masters: false,
            use_atomic_slot_migration: false,
            color: std::env::var("TERM").is_ok_and(|t| t.contains("xterm")),
            verbose: o.verbose,
        };
        for (flag, value) in &o.modes.cluster_flags {
            let v = value.clone().unwrap_or_default();
            match flag.as_slice() {
                b"--cluster-replicas" => c.replicas = atoi(&v),
                b"--cluster-master-id" | b"--cluster-primary-id" => c.master_id = Some(v),
                b"--cluster-from" => c.from = Some(v),
                b"--cluster-to" => c.to = Some(v),
                b"--cluster-from-user" => c.from_user = Some(v),
                b"--cluster-from-pass" => c.from_pass = Some(v),
                b"--cluster-from-askpass" => c.from_askpass = true,
                b"--cluster-slots" => c.slots = atoi(&v),
                b"--cluster-timeout" => c.timeout_ms = Some(atoi(&v)),
                b"--cluster-pipeline" => c.pipeline = atoi(&v),
                b"--cluster-threshold" => c.threshold = atof(&v),
                b"--cluster-weight" => c.weights.push(v),
                b"--cluster-yes" => c.yes = true,
                b"--cluster-only-masters" | b"--cluster-only-primaries" => c.only_masters = true,
                b"--cluster-only-replicas" => c.only_replicas = true,
                b"--cluster-simulate" => c.simulate = true,
                b"--cluster-replace" => c.replace = true,
                b"--cluster-copy" => c.copy = true,
                b"--cluster-slave" | b"--cluster-replica" => c.replica = true,
                b"--cluster-use-empty-masters" | b"--cluster-use-empty-primaries" => {
                    c.use_empty_masters = true;
                }
                b"--cluster-search-multiple-owners" => c.search_multiple_owners = true,
                b"--cluster-fix-with-unreachable-masters"
                | b"--cluster-fix-with-unreachable-primaries" => {
                    c.fix_with_unreachable_masters = true
                }
                b"--cluster-use-atomic-slot-migration" => c.use_atomic_slot_migration = true,
                _ => {}
            }
        }
        c
    }
}

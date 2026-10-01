//! `doctor` — run every table's `VERIFY` and turn the counters into an
//! exit code.
//!
//! Lesson 8 of the migration playbook: *make `VERIFY` part of
//! operations, not part of the migration.* The counters are fresh on
//! every call and cheap enough for a cron; what was missing is the
//! shell that turns them into something a cron can act on.
//!
//! The mapping is the lesson's own words, not a new opinion:
//!
//! * `drift` and `missing` **should be zero forever** — non-zero is a
//!   failure;
//! * non-zero `duplicates` on an ORDERPATH means **pagination needs a
//!   bounded tie-break** — a warning about a design choice, not a
//!   corruption;
//! * `absent` / `excluded` / `coerce_failures` **name the rows each
//!   exclusion cause claimed** — reported, never failed on, because
//!   every one of them is a legitimate state.
//!
//! And one thing the lesson could not have known: `TABLE.VERIFY`
//! answers `-INDEXBUILDING` while a backfill is still running. A cron
//! that read that as a failure would page someone every time an index
//! was declared, so it is its own outcome.
//!
//! ```
//! use kevy_cli::doctor::{Health, check_table, table_names};
//! # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
//! let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
//! client.request_borrowed(&[
//!     b"TABLE.DECLARE", b"users", b"PREFIX", b"user:", b"PK", b"id",
//!     b"COLUMN", b"id", b"i64", b"COLUMN", b"age", b"i64", b"INDEX", b"age", b"range",
//! ])?;
//! client.request_borrowed(&[b"HSET", b"user:1", b"id", b"1", b"age", b"30"])?;
//! client.request_borrowed(&[b"HSET", b"user:2", b"id", b"2", b"age", b"41"])?;
//! for name in table_names(&mut client)? {
//!     let h = check_table(&mut client, &name)?;
//!     assert_eq!(h.health, Health::Ok, "{name}: {}", h.reported);
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::io;
use std::process::ExitCode;

use crate::link::Link;
use kevy_resp_client::Reply;

mod verify;
use verify::{classify, listed_names, report};
pub(crate) use verify::{fields, run_on};

/// What `doctor` concluded about one table.
///
/// ```
/// use kevy_cli::doctor::{Health, check_table};
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// # client.request_borrowed(&[b"TABLE.DECLARE", b"users", b"PREFIX", b"user:", b"PK", b"id",
/// #     b"COLUMN", b"id", b"i64", b"COLUMN", b"age", b"i64", b"INDEX", b"age", b"range"])?;
/// # client.request_borrowed(&[b"HSET", b"user:1", b"id", b"1", b"age", b"30"])?;
/// # client.request_borrowed(&[b"HSET", b"user:2", b"id", b"2", b"age", b"41"])?;
/// // what a cron wrapper does with each verdict
/// let pages_someone = |h: &Health| matches!(h, Health::Drift { .. });
/// assert!(!pages_someone(&check_table(&mut client, "users")?.health));
/// assert!(pages_someone(&check_table(&mut client, "no-such-table")?.health));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Health {
    /// Every counter where it should be.
    ///
    /// ```
    /// use kevy_cli::doctor::{Health, check_table};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # client.request_borrowed(&[b"TABLE.DECLARE", b"users", b"PREFIX", b"user:", b"PK", b"id",
    /// #     b"COLUMN", b"id", b"i64", b"COLUMN", b"age", b"i64", b"INDEX", b"age", b"range"])?;
    /// client.request_borrowed(&[b"HSET", b"user:1", b"id", b"1", b"age", b"30"])?;
    /// client.request_borrowed(&[b"HSET", b"user:2", b"id", b"2", b"age", b"41"])?;
    /// assert_eq!(check_table(&mut client, "users")?.health, Health::Ok);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Ok,
    /// `drift` or `missing` is non-zero — the index and the keyspace
    /// disagree, which is the thing VERIFY exists to make falsifiable.
    ///
    /// ```
    /// use kevy_cli::doctor::{Health, check_table};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// // a VERIFY that cannot be answered is drift, never a pass
    /// let h = check_table(&mut client, "orders")?;
    /// assert!(matches!(h.health, Health::Drift { .. }));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Drift {
        /// Which counters were non-zero, with their values.
        ///
        /// ```
        /// use kevy_cli::doctor::{Health, check_table};
        /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
        /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
        /// let Health::Drift { detail } = check_table(&mut client, "orders")?.health else {
        ///     panic!("an undeclared table cannot verify clean");
        /// };
        /// assert!(detail.contains("no such table"), "{detail}");
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        detail: String,
    },
    /// Non-zero `duplicates`: not corruption, but pagination over this
    /// path needs a bounded tie-break or pages will repeat rows.
    ///
    /// ```
    /// use kevy_cli::doctor::{Health, check_table};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # client.request_borrowed(&[b"TABLE.DECLARE", b"users", b"PREFIX", b"user:", b"PK", b"id",
    /// #     b"COLUMN", b"id", b"i64", b"COLUMN", b"age", b"i64", b"INDEX", b"age", b"range"])?;
    /// # client.request_borrowed(&[b"HSET", b"user:1", b"id", b"1", b"age", b"30"])?;
    /// # client.request_borrowed(&[b"HSET", b"user:2", b"id", b"2", b"age", b"41"])?;
    /// client.request_borrowed(&[b"HSET", b"user:3", b"id", b"3", b"age", b"30"])?; // a second 30
    /// let h = check_table(&mut client, "users")?;
    /// assert!(matches!(h.health, Health::NeedsTieBreak { .. }), "{:?}", h.health);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    NeedsTieBreak {
        /// How many duplicate order values were found.
        ///
        /// ```
        /// use kevy_cli::doctor::{Health, check_table};
        /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
        /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
        /// # client.request_borrowed(&[b"TABLE.DECLARE", b"users", b"PREFIX", b"user:", b"PK", b"id",
        /// #     b"COLUMN", b"id", b"i64", b"COLUMN", b"age", b"i64", b"INDEX", b"age", b"range"])?;
        /// # client.request_borrowed(&[b"HSET", b"user:1", b"id", b"1", b"age", b"30"])?;
        /// # client.request_borrowed(&[b"HSET", b"user:2", b"id", b"2", b"age", b"41"])?;
        /// client.request_borrowed(&[b"HSET", b"user:3", b"id", b"3", b"age", b"30"])?; // a second 30
        /// let h = check_table(&mut client, "users")?;
        /// assert_eq!(h.health, Health::NeedsTieBreak { duplicates: 1 });
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        duplicates: u64,
    },
    /// A backfill is still running. Not a verdict either way.
    ///
    /// ```
    /// use kevy_cli::doctor::Health;
    /// // an index still backfilling says nothing about drift: wait and ask again
    /// let retry_later = |h: &Health| matches!(h, Health::Building);
    /// assert!(retry_later(&Health::Building));
    /// assert!(!retry_later(&Health::Ok));
    /// ```
    Building,
}

/// One table's name and what was concluded about it.
///
/// ```
/// use kevy_cli::doctor::{Health, check_table};
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// # client.request_borrowed(&[b"TABLE.DECLARE", b"users", b"PREFIX", b"user:", b"PK", b"id",
/// #     b"COLUMN", b"id", b"i64", b"COLUMN", b"age", b"i64", b"INDEX", b"age", b"range"])?;
/// # client.request_borrowed(&[b"HSET", b"user:1", b"id", b"1", b"age", b"30"])?;
/// # client.request_borrowed(&[b"HSET", b"user:2", b"id", b"2", b"age", b"41"])?;
/// let h = check_table(&mut client, "users")?;
/// println!("{}  {:?}  ({})", h.name, h.health, h.reported);
/// assert_eq!((h.name.as_str(), h.health), ("users", Health::Ok));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct TableHealth {
    /// The declared table name.
    ///
    /// ```
    /// use kevy_cli::doctor::check_table;
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # client.request_borrowed(&[b"TABLE.DECLARE", b"users", b"PREFIX", b"user:", b"PK", b"id",
    /// #     b"COLUMN", b"id", b"i64", b"COLUMN", b"age", b"i64", b"INDEX", b"age", b"range"])?;
    /// assert_eq!(check_table(&mut client, "users")?.name, "users");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub name: String,
    /// The verdict.
    ///
    /// ```
    /// use kevy_cli::doctor::{Health, check_table};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # client.request_borrowed(&[b"TABLE.DECLARE", b"users", b"PREFIX", b"user:", b"PK", b"id",
    /// #     b"COLUMN", b"id", b"i64", b"COLUMN", b"age", b"i64", b"INDEX", b"age", b"range"])?;
    /// # client.request_borrowed(&[b"HSET", b"user:1", b"id", b"1", b"age", b"30"])?;
    /// # client.request_borrowed(&[b"HSET", b"user:2", b"id", b"2", b"age", b"41"])?;
    /// assert_eq!(check_table(&mut client, "users")?.health, Health::Ok);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub health: Health,
    /// The counters worth showing whatever the verdict — the exclusion
    /// causes, which are legitimate states rather than problems.
    ///
    /// ```
    /// use kevy_cli::doctor::check_table;
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # client.request_borrowed(&[b"TABLE.DECLARE", b"users", b"PREFIX", b"user:", b"PK", b"id",
    /// #     b"COLUMN", b"id", b"i64", b"COLUMN", b"age", b"i64", b"INDEX", b"age", b"range"])?;
    /// # client.request_borrowed(&[b"HSET", b"user:1", b"id", b"1", b"age", b"30"])?;
    /// # client.request_borrowed(&[b"HSET", b"user:2", b"id", b"2", b"age", b"41"])?;
    /// let h = check_table(&mut client, "users")?;
    /// assert!(h.reported.starts_with("rows 2 · entries 2"), "{}", h.reported);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub reported: String,
}

/// Every declared table's name, in declaration order.
///
/// ```
/// use kevy_cli::doctor::table_names;
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// assert!(table_names(&mut client)?.is_empty());
/// client.request_borrowed(&[
///     b"TABLE.DECLARE", b"users", b"PREFIX", b"user:", b"PK", b"id",
///     b"COLUMN", b"id", b"i64", b"COLUMN", b"age", b"i64", b"INDEX", b"age", b"range",
/// ])?;
/// assert_eq!(table_names(&mut client)?, ["users"]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn table_names(client: &mut dyn Link) -> io::Result<Vec<String>> {
    let Reply::Array(tables) = client.request_borrowed(&[b"TABLE.LIST"])? else {
        return Ok(Vec::new());
    };
    Ok(tables
        .iter()
        .filter_map(|t| {
            let Reply::Array(items) = t else { return None };
            fields(items).into_iter().find(|(k, _)| k == "name").map(|(_, v)| v)
        })
        .collect())
}

/// Verify one table and read its counters against lesson 8's mapping.
///
/// ```
/// use kevy_cli::doctor::{Health, check_table};
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// # client.request_borrowed(&[b"TABLE.DECLARE", b"users", b"PREFIX", b"user:", b"PK", b"id",
/// #     b"COLUMN", b"id", b"i64", b"COLUMN", b"age", b"i64", b"INDEX", b"age", b"range"])?;
/// # client.request_borrowed(&[b"HSET", b"user:1", b"id", b"1", b"age", b"30"])?;
/// # client.request_borrowed(&[b"HSET", b"user:2", b"id", b"2", b"age", b"41"])?;
/// let h = check_table(&mut client, "users")?;
/// assert_eq!(h.health, Health::Ok);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn check_table(client: &mut dyn Link, name: &str) -> io::Result<TableHealth> {
    check_with(client, b"TABLE.VERIFY", name)
}

/// The same reading for `IDX.VERIFY` or `VIEW.VERIFY`, whose replies are
/// one group of counters rather than one per index.
fn check_with(client: &mut dyn Link, verb: &[u8], name: &str) -> io::Result<TableHealth> {
    let reply = client.request_borrowed(&[verb, name.as_bytes()])?;
    let reply = match reply {
        Reply::Array(items) if matches!(items.first(), Some(Reply::Bulk(_))) => {
            Reply::Array(vec![Reply::Array(items)])
        }
        other => other,
    };
    if let Reply::Error(e) = &reply {
        let msg = String::from_utf8_lossy(e);
        let health = if msg.starts_with("INDEXBUILDING") {
            Health::Building
        } else {
            Health::Drift { detail: msg.into_owned() }
        };
        return Ok(TableHealth { name: name.to_string(), health, reported: String::new() });
    }
    // The reply is per-index groups plus a spot-check group; summing the
    // counters across groups is the table-level answer.
    let Reply::Array(groups) = reply else {
        return Ok(TableHealth {
            name: name.to_string(),
            health: Health::Drift { detail: "unreadable VERIFY reply".into() },
            reported: String::new(),
        });
    };
    let mut sums: std::collections::BTreeMap<String, u64> = Default::default();
    for g in &groups {
        if let Reply::Array(items) = g {
            for (k, v) in fields(items) {
                if let Ok(n) = v.parse::<u64>() {
                    *sums.entry(k).or_insert(0) += n;
                }
            }
        }
    }
    let get = |k: &str| sums.get(k).copied().unwrap_or(0);
    let reported = format!(
        "rows {} · entries {} · absent {} · excluded {} · coerce_failures {}",
        get("rows"),
        get("entries"),
        get("absent"),
        get("excluded"),
        get("coerce_failures")
    );
    Ok(TableHealth { name: name.to_string(), health: classify(&groups), reported })
}

/// Check every table and print one line each. Exit non-zero only on
/// drift, unless `on_warning` says a warning fails too — by default a
/// warning is information, and a cron that fails on information stops
/// being read.
///
/// ```
/// use kevy_cli::doctor::{OnWarning, run};
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// # client.request_borrowed(&[b"TABLE.DECLARE", b"users", b"PREFIX", b"user:", b"PK", b"id",
/// #     b"COLUMN", b"id", b"i64", b"COLUMN", b"age", b"i64", b"INDEX", b"age", b"range"])?;
/// # client.request_borrowed(&[b"HSET", b"user:1", b"id", b"1", b"age", b"30"])?;
/// # client.request_borrowed(&[b"HSET", b"user:2", b"id", b"2", b"age", b"41"])?;
/// let code = run(&mut client, OnWarning::Report)?; // prints one line per table
/// assert_eq!(code, std::process::ExitCode::SUCCESS);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn run(client: &mut dyn Link, on_warning: OnWarning) -> io::Result<ExitCode> {
    run_scoped(client, on_warning, Scope::default())
}

/// What a warning (duplicates on an ORDERPATH) does to `doctor`'s exit
/// code. Drift fails either way.
///
/// ```
/// use kevy_cli::doctor::OnWarning;
/// // `doctor --warn-is-failure`
/// assert_ne!(OnWarning::Fail, OnWarning::default());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum OnWarning {
    /// Print the warning and still exit successfully.
    #[default]
    ///
    /// ```
    /// use kevy_cli::doctor::{OnWarning, run};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # client.request_borrowed(&[b"TABLE.DECLARE", b"users", b"PREFIX", b"user:", b"PK", b"id",
    /// #     b"COLUMN", b"id", b"i64", b"COLUMN", b"age", b"i64", b"INDEX", b"age", b"range"])?;
    /// # client.request_borrowed(&[b"HSET", b"user:1", b"id", b"1", b"age", b"30"])?;
    /// # client.request_borrowed(&[b"HSET", b"user:2", b"id", b"2", b"age", b"41"])?;
    /// client.request_borrowed(&[b"HSET", b"user:3", b"id", b"3", b"age", b"30"])?; // a second 30
    /// // the duplicate warns, and the cron still succeeds
    /// assert_eq!(run(&mut client, OnWarning::Report)?, std::process::ExitCode::SUCCESS);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Report,
    /// Exit non-zero when anything warned (`--warn-is-failure`).
    ///
    /// ```
    /// use kevy_cli::doctor::{OnWarning, run};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # client.request_borrowed(&[b"TABLE.DECLARE", b"users", b"PREFIX", b"user:", b"PK", b"id",
    /// #     b"COLUMN", b"id", b"i64", b"COLUMN", b"age", b"i64", b"INDEX", b"age", b"range"])?;
    /// # client.request_borrowed(&[b"HSET", b"user:1", b"id", b"1", b"age", b"30"])?;
    /// # client.request_borrowed(&[b"HSET", b"user:2", b"id", b"2", b"age", b"41"])?;
    /// client.request_borrowed(&[b"HSET", b"user:3", b"id", b"3", b"age", b"30"])?; // a second 30
    /// assert_eq!(run(&mut client, OnWarning::Fail)?, std::process::ExitCode::FAILURE);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Fail,
}

/// What `doctor` verifies besides tables. The default is tables only.
///
/// ```
/// use kevy_cli::doctor::Scope;
/// // `doctor --indexes --views`
/// let everything = Scope::default().with_indexes(true).with_views(true);
/// assert!(everything.indexes && everything.views);
/// assert!(!Scope::default().indexes);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct Scope {
    /// Indexes declared on their own (not compiled from a table).
    ///
    /// ```
    /// // `doctor --indexes`: an index `users.age` belongs to table `users`
    /// // and is verified with it; only an index no table compiled is added.
    /// let s = kevy_cli::doctor::Scope::default().with_indexes(true);
    /// assert!(s.indexes);
    /// ```
    pub indexes: bool,
    /// Views.
    ///
    /// ```
    /// // `doctor --views`: VIEW.VERIFY for every view VIEW.LIST names.
    /// let s = kevy_cli::doctor::Scope::default().with_views(true);
    /// assert!(s.views);
    /// ```
    pub views: bool,
}

impl Scope {
    /// Also verify indexes no table compiled (`--indexes`).
    ///
    /// ```
    /// let s = kevy_cli::doctor::Scope::default().with_indexes(true);
    /// assert!(s.indexes && !s.views);
    /// ```
    pub fn with_indexes(mut self, indexes: bool) -> Self {
        self.indexes = indexes;
        self
    }

    /// Also verify every view (`--views`).
    ///
    /// ```
    /// let s = kevy_cli::doctor::Scope::default().with_views(true);
    /// assert!(s.views && !s.indexes);
    /// ```
    pub fn with_views(mut self, views: bool) -> Self {
        self.views = views;
        self
    }
}

/// [`run`], also verifying bare indexes and views when `scope` says so.
///
/// ```
/// use kevy_cli::doctor::{OnWarning, Scope, run_scoped};
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// # client.request_borrowed(&[b"TABLE.DECLARE", b"users", b"PREFIX", b"user:", b"PK", b"id",
/// #     b"COLUMN", b"id", b"i64", b"COLUMN", b"age", b"i64", b"INDEX", b"age", b"range"])?;
/// # client.request_borrowed(&[b"HSET", b"user:1", b"id", b"1", b"age", b"30"])?;
/// # client.request_borrowed(&[b"HSET", b"user:2", b"id", b"2", b"age", b"41"])?;
/// let scope = Scope::default().with_indexes(true).with_views(true);
/// let code = run_scoped(&mut client, OnWarning::Report, scope)?;
/// assert_eq!(code, std::process::ExitCode::SUCCESS);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn run_scoped(
    client: &mut dyn Link,
    on_warning: OnWarning,
    scope: Scope,
) -> io::Result<ExitCode> {
    let tables = table_names(client)?;
    let mut targets: Vec<(&[u8], &str, String)> =
        tables.iter().map(|n| (&b"TABLE.VERIFY"[..], "", n.clone())).collect();
    if scope.indexes {
        let bare = listed_names(client, b"IDX.LIST")?
            .into_iter()
            .filter(|n| !tables.iter().any(|t| n.starts_with(&format!("{t}."))));
        targets.extend(bare.map(|n| (&b"IDX.VERIFY"[..], "index ", n)));
    }
    if scope.views {
        targets.extend(
            listed_names(client, b"VIEW.LIST")?
                .into_iter()
                .map(|n| (&b"VIEW.VERIFY"[..], "view ", n)),
        );
    }
    if targets.is_empty() {
        // Tables only: the words the migration playbook quotes.
        let what =
            if scope.indexes || scope.views { "nothing declared" } else { "no tables declared" };
        println!("doctor: {what} — nothing to verify");
        return Ok(ExitCode::SUCCESS);
    }
    let noun = if scope.indexes || scope.views { "checked" } else { "table(s)" };
    report(client, &targets, on_warning, noun)
}

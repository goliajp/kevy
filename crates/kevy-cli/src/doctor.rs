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

use std::io;
use std::process::ExitCode;

use kevy_resp_client::{Reply, RespClient};

/// What `doctor` concluded about one table.
pub enum Health {
    /// Every counter where it should be.
    Ok,
    /// `drift` or `missing` is non-zero — the index and the keyspace
    /// disagree, which is the thing VERIFY exists to make falsifiable.
    Drift {
        /// Which counters were non-zero, with their values.
        detail: String,
    },
    /// Non-zero `duplicates`: not corruption, but pagination over this
    /// path needs a bounded tie-break or pages will repeat rows.
    NeedsTieBreak {
        /// How many duplicate order values were found.
        duplicates: u64,
    },
    /// A backfill is still running. Not a verdict either way.
    Building,
}

/// One table's name and what was concluded about it.
pub struct TableHealth {
    /// The declared table name.
    pub name: String,
    /// The verdict.
    pub health: Health,
    /// The counters worth showing whatever the verdict — the exclusion
    /// causes, which are legitimate states rather than problems.
    pub reported: String,
}

/// Pull `[field, value, …]` pairs out of a flat reply array.
pub(crate) fn fields(items: &[Reply]) -> Vec<(String, String)> {
    let bulks: Vec<String> = items
        .iter()
        .map(|r| match r {
            Reply::Bulk(b) => String::from_utf8_lossy(b).into_owned(),
            Reply::Int(i) => i.to_string(),
            _ => String::new(),
        })
        .collect();
    bulks.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0].clone(), c[1].clone())).collect()
}

/// Every declared table's name, in declaration order.
pub fn table_names(client: &mut RespClient) -> io::Result<Vec<String>> {
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
pub fn check_table(client: &mut RespClient, name: &str) -> io::Result<TableHealth> {
    let reply = client.request_borrowed(&[b"TABLE.VERIFY", name.as_bytes()])?;
    if let Reply::Error(e) = &reply {
        let msg = String::from_utf8_lossy(e);
        let health =
            if msg.starts_with("INDEXBUILDING") { Health::Building } else { Health::Drift { detail: msg.into_owned() } };
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

/// Lesson 8's mapping, as one function so a test can state it without a
/// server: zero-forever counters fail, duplicates warn, exclusion
/// causes are reported and never fail.
fn classify(groups: &[Reply]) -> Health {
    let mut sums: std::collections::BTreeMap<String, u64> = Default::default();
    for g in groups {
        if let Reply::Array(items) = g {
            for (k, v) in fields(items) {
                if let Ok(n) = v.parse::<u64>() {
                    *sums.entry(k).or_insert(0) += n;
                }
            }
        }
    }
    let get = |k: &str| sums.get(k).copied().unwrap_or(0);
    let (drift, missing, dups) = (get("drift"), get("missing"), get("duplicates"));
    if drift > 0 || missing > 0 {
        Health::Drift { detail: format!("drift {drift}, missing {missing}") }
    } else if dups > 0 {
        Health::NeedsTieBreak { duplicates: dups }
    } else {
        Health::Ok
    }
}

/// Check every table and print one line each. Exit non-zero only on
/// drift — a warning is information, and a cron that fails on
/// information stops being read.
pub fn run(client: &mut RespClient, warn_is_failure: bool) -> io::Result<ExitCode> {
    let names = table_names(client)?;
    if names.is_empty() {
        println!("doctor: no tables declared — nothing to verify");
        return Ok(ExitCode::SUCCESS);
    }
    let (mut bad, mut warned, mut building) = (0u32, 0u32, 0u32);
    for name in &names {
        let h = check_table(client, name)?;
        match &h.health {
            Health::Ok => println!("  OK       {name}  ({})", h.reported),
            Health::Building => {
                building += 1;
                println!("  BUILDING {name}  — an index is still backfilling, not a verdict");
            }
            Health::NeedsTieBreak { duplicates } => {
                warned += 1;
                println!(
                    "  WARN     {name}  duplicates {duplicates} — paging this path needs a \
                     bounded tie-break or pages repeat rows  ({})",
                    h.reported
                );
            }
            Health::Drift { detail } => {
                bad += 1;
                println!("  DRIFT    {name}  {detail}  ({})", h.reported);
            }
        }
    }
    println!(
        "doctor: {} table(s) — {bad} drifted, {warned} warned, {building} still building",
        names.len()
    );
    Ok(if bad > 0 || (warn_is_failure && warned > 0) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// `doctor [-h host] [-p port] [--warn-is-failure]`
pub fn run_doctor_cli(args: &[String]) -> ExitCode {
    let (mut host, mut port) = (crate::DEFAULT_HOST.to_string(), crate::DEFAULT_PORT);
    let mut strict = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" if i + 1 < args.len() => {
                host = args[i + 1].clone();
                i += 2;
            }
            "-p" if i + 1 < args.len() => {
                port = args[i + 1].parse().unwrap_or(crate::DEFAULT_PORT);
                i += 2;
            }
            "--warn-is-failure" => {
                strict = true;
                i += 1;
            }
            _ => i += 1,
        }
    }
    let mut client = match RespClient::connect(&host, port) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kevy-cli: could not connect to {host}:{port}: {e}");
            return ExitCode::FAILURE;
        }
    };
    match run(&mut client, strict) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("kevy-cli doctor: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arr(pairs: &[(&str, i64)]) -> Reply {
        let mut items = Vec::new();
        for (k, v) in pairs {
            items.push(Reply::Bulk(k.as_bytes().to_vec()));
            items.push(Reply::Bulk(v.to_string().into_bytes()));
        }
        Reply::Array(items)
    }

    /// The counters that must be zero forever are the ones that fail.
    #[test]
    fn drift_and_missing_are_the_failing_counters() {
        for k in ["drift", "missing"] {
            let sums = [("rows", 10), (k, 1)];
            let groups = vec![arr(&sums)];
            let health = classify(&groups);
            assert!(matches!(health, Health::Drift { .. }), "{k} must fail");
        }
    }

    /// Duplicates are a design signal, not corruption — the lesson says
    /// it means pagination needs a bounded tie-break.
    #[test]
    fn duplicates_warn_rather_than_fail() {
        let groups = vec![arr(&[("rows", 10), ("duplicates", 3), ("drift", 0)])];
        assert!(matches!(classify(&groups), Health::NeedsTieBreak { duplicates: 3 }));
    }

    /// Every exclusion cause is a legitimate state. A doctor that failed
    /// on them would be red on any table with a NULL column.
    #[test]
    fn exclusion_causes_never_fail() {
        let groups =
            vec![arr(&[("rows", 10), ("absent", 4), ("excluded", 2), ("coerce_failures", 1)])];
        assert!(matches!(classify(&groups), Health::Ok));
    }
}

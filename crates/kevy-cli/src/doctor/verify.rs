//! Reading VERIFY replies into verdicts, and the command-line face of
//! `doctor`.

use std::io;
use std::process::ExitCode;

use super::{Health, OnWarning, Scope, check_with, run_scoped};
use crate::link::Link;
use kevy_resp_client::Reply;

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

/// Lesson 8's mapping, as one function so a test can state it without a
/// server: zero-forever counters fail, duplicates warn, exclusion
/// causes are reported and never fail.
pub(super) fn classify(groups: &[Reply]) -> Health {
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
    if get("rebuilding") > 0 {
        Health::Building
    } else if drift > 0 || missing > 0 {
        Health::Drift { detail: format!("drift {drift}, missing {missing}") }
    } else if dups > 0 {
        Health::NeedsTieBreak { duplicates: dups }
    } else {
        Health::Ok
    }
}

/// The `name` of every row a LIST verb answers.
pub(super) fn listed_names(client: &mut dyn Link, verb: &[u8]) -> io::Result<Vec<String>> {
    let Reply::Array(rows) = client.request_borrowed(&[verb])? else { return Ok(Vec::new()) };
    Ok(rows
        .iter()
        .filter_map(|r| {
            let Reply::Array(items) = r else { return None };
            fields(items).into_iter().find(|(k, _)| k == "name").map(|(_, v)| v)
        })
        .collect())
}

pub(super) fn report(
    client: &mut dyn Link,
    targets: &[(&[u8], &str, String)],
    on_warning: OnWarning,
    noun: &str,
) -> io::Result<ExitCode> {
    let (mut bad, mut warned, mut building) = (0u32, 0u32, 0u32);
    for (verb, kind, bare) in targets {
        let h = check_with(client, verb, bare)?;
        let name = format!("{kind}{bare}");
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
        "doctor: {} {noun} — {bad} drifted, {warned} warned, {building} still building",
        targets.len()
    );
    Ok(if bad > 0 || (on_warning == OnWarning::Fail && warned > 0) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// `doctor [--warn-is-failure] [--indexes] [--views]` on `client`.
pub(crate) fn run_on(client: &mut dyn Link, args: &[String]) -> ExitCode {
    let mut on_warning = OnWarning::Report;
    let mut scope = Scope::default();
    for word in args {
        match word.as_str() {
            "--warn-is-failure" => on_warning = OnWarning::Fail,
            "--indexes" => scope.indexes = true,
            "--views" => scope.views = true,
            other => {
                eprintln!("kevy-cli doctor: {}", crate::tools::argscan::unexpected(other));
                eprintln!(
                    "usage: kevy-cli --kevy doctor [--warn-is-failure] [--indexes] [--views]"
                );
                return ExitCode::FAILURE;
            }
        }
    }
    match run_scoped(client, on_warning, scope) {
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

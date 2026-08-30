//! `shadow` — run the old query and the new one side by side and say
//! where they disagree.
//!
//! Lesson 4 of the migration playbook, which is the one that decides
//! whether anyone dares cut over: *serve reads from the old path while
//! computing the new answer beside it, and compare the **order** too,
//! not just the membership.* Score drift produces identical sets in
//! different orders, and a paginated UI turns that into user-visible
//! churn.
//!
//! It also carries lesson 2 without being asked to. A writer nobody
//! remembered to update shows up here as rows the new path is missing —
//! which is the same signal `TABLE.VERIFY` reports after the fact, seen
//! before the cutover instead of after.

use std::io;

use std::process::ExitCode;

use kevy_resp_client::{Reply, RespClient};

/// One side's reading of a reply: the row keys in order, each with the
/// sort value it was ordered by (empty when the shape does not carry
/// one).
type Rows = Vec<(Vec<u8>, Vec<u8>)>;

/// How to read a reply into rows. Guessing is not free here — reading
/// `ZRANGE … WITHSCORES` as a plain list silently treats every score as
/// a row key and reports a divergence on every sample — so the two
/// ambiguous shapes are told apart by the caller, not by a heuristic.
#[derive(Clone, Copy, PartialEq)]
pub enum Shape {
    /// `[cursor, [key, sortval, key, sortval, …]]` — kevy's paged
    /// index reply. Detected, not declared: a two-element array whose
    /// second element is an array cannot be anything else here.
    Paged,
    /// `[a, b, c, …]` — every element is a row key.
    Flat,
    /// `[member, score, member, score, …]` — `WITHSCORES` and friends.
    Pairs,
}

/// Read a reply into ordered rows under `shape`. `Paged` is recognised
/// from the reply itself, so passing `Flat` for a kevy index reply
/// still does the right thing rather than reporting nonsense.
pub fn rows_of(reply: &Reply, shape: Shape) -> Rows {
    let Reply::Array(items) = reply else { return Vec::new() };
    if let [Reply::Bulk(_), Reply::Array(inner)] = items.as_slice() {
        return pairs(inner);
    }
    match shape {
        Shape::Paged | Shape::Pairs => pairs(items),
        Shape::Flat => items
            .iter()
            .filter_map(|r| match r {
                Reply::Bulk(b) => Some((b.clone(), Vec::new())),
                _ => None,
            })
            .collect(),
    }
}

fn pairs(items: &[Reply]) -> Rows {
    let bulks: Vec<&Vec<u8>> =
        items.iter().filter_map(|r| if let Reply::Bulk(b) = r { Some(b) } else { None }).collect();
    bulks
        .chunks(2)
        .map(|c| (c[0].clone(), c.get(1).map(|v| (*v).clone()).unwrap_or_default()))
        .collect()
}

/// What one comparison found.
pub struct Divergence {
    /// Position of the first place the two orders differ.
    pub at: usize,
    /// The old side's row and the value it was ordered by.
    pub old: Option<(Vec<u8>, Vec<u8>)>,
    /// The new side's, at the same position.
    pub new: Option<(Vec<u8>, Vec<u8>)>,
}

/// Rows the new side lacks, rows it invents, and the first ordering
/// difference. Membership and order are reported separately because
/// they fail for different reasons: a missing row is a writer nobody
/// updated, a reordering is score drift.
pub struct Compared {
    /// Rows the old path returns and the new one does not.
    pub missing: Vec<Vec<u8>>,
    /// Rows the new path returns and the old one does not.
    pub extra: Vec<Vec<u8>>,
    /// The first position where the two orders differ, if any.
    pub first: Option<Divergence>,
}

/// Compare two readings: what is missing, what is extra, and where the
/// orders first part company.
pub fn compare(old: &Rows, new: &Rows) -> Compared {
    let old_set: std::collections::HashSet<&[u8]> = old.iter().map(|(k, _)| k.as_slice()).collect();
    let new_set: std::collections::HashSet<&[u8]> = new.iter().map(|(k, _)| k.as_slice()).collect();
    let missing = old
        .iter()
        .filter(|(k, _)| !new_set.contains(k.as_slice()))
        .map(|(k, _)| k.clone())
        .collect();
    let extra = new
        .iter()
        .filter(|(k, _)| !old_set.contains(k.as_slice()))
        .map(|(k, _)| k.clone())
        .collect();
    let mut first = None;
    for i in 0..old.len().max(new.len()) {
        if old.get(i).map(|(k, _)| k) != new.get(i).map(|(k, _)| k) {
            first = Some(Divergence { at: i, old: old.get(i).cloned(), new: new.get(i).cloned() });
            break;
        }
    }
    Compared { missing, extra, first }
}

/// Outcome of a shadow run — the paste-able conclusion.
pub struct ShadowReport {
    /// How many times both sides were asked.
    pub samples: u64,
    /// How many of those disagreed in membership or order.
    pub diverged: u64,
    /// The first sample that disagreed, and how.
    pub first: Option<(u64, Compared)>,
}

/// Run both commands `samples` times and compare each pair.
///
/// Both sides are issued on the same connection, back to back, so the
/// window between them is as small as this can make it. A row written
/// between the two reads shows up as a divergence, which is why a
/// single disagreement is a lead rather than a verdict — the report
/// carries the count so a rate can be read off it.
pub fn run(
    client: &mut RespClient,
    old_cmd: &[Vec<u8>],
    new_cmd: &[Vec<u8>],
    old_shape: Shape,
    new_shape: Shape,
    samples: u64,
) -> io::Result<ShadowReport> {
    let mut report = ShadowReport { samples: 0, diverged: 0, first: None };
    for i in 0..samples {
        let old_ref: Vec<&[u8]> = old_cmd.iter().map(|a| a.as_slice()).collect();
        let new_ref: Vec<&[u8]> = new_cmd.iter().map(|a| a.as_slice()).collect();
        let old = rows_of(&client.request_borrowed(&old_ref)?, old_shape);
        let new = rows_of(&client.request_borrowed(&new_ref)?, new_shape);
        report.samples += 1;
        let c = compare(&old, &new);
        if !c.missing.is_empty() || !c.extra.is_empty() || c.first.is_some() {
            report.diverged += 1;
            if report.first.is_none() {
                report.first = Some((i, c));
            }
        }
    }
    Ok(report)
}

/// Print the report the way lesson 4 asks for: the first divergence
/// with **both** sort keys, because that one line names the drifting
/// writer.
pub fn print_report(r: &ShadowReport) {
    let show = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    match &r.first {
        None => println!(
            "shadow: {} samples, 0 divergences — the new path answers what the old one does",
            r.samples
        ),
        Some((n, c)) => {
            println!(
                "shadow: {} samples, {} diverged (first at sample {})",
                r.samples, r.diverged, n
            );
            if !c.missing.is_empty() {
                println!(
                    "  MISSING from the new path ({}): {}",
                    c.missing.len(),
                    c.missing.iter().take(5).map(|k| show(k)).collect::<Vec<_>>().join(", ")
                );
                println!("    a row the old path has and the new one does not is usually a writer");
                println!(
                    "    that was never updated — the same class TABLE.VERIFY's `missing` finds"
                );
            }
            if !c.extra.is_empty() {
                println!(
                    "  EXTRA in the new path ({}): {}",
                    c.extra.len(),
                    c.extra.iter().take(5).map(|k| show(k)).collect::<Vec<_>>().join(", ")
                );
            }
            if let Some(d) = &c.first {
                let side = |x: &Option<(Vec<u8>, Vec<u8>)>| match x {
                    Some((k, v)) if v.is_empty() => show(k),
                    Some((k, v)) => format!("{} (sort {})", show(k), show(v)),
                    None => "<past the end>".to_string(),
                };
                println!("  ORDER differs at position {}:", d.at);
                println!("    old: {}", side(&d.old));
                println!("    new: {}", side(&d.new));
                println!("    identical sets in different orders is score drift, and a paged UI");
                println!("    shows it to users as churn — compare the two sort values above");
            }
        }
    }
}

/// `shadow [-h host] [-p port] --old "<cmd>" --new "<cmd>"
/// [--old-pairs] [--new-flat] [--samples n]`
///
/// Both sides are whole commands, quoted, because the old path is
/// whatever the application already runs — a ZRANGE, an LRANGE, a
/// SMEMBERS — and the new one is an IDX.QUERY. Nothing here knows
/// which; it compares the two orders of row keys they produce.
/// Everything `shadow` takes from the command line.
struct ShadowArgs {
    host: String,
    port: u16,
    old: Option<String>,
    new: Option<String>,
    old_shape: Shape,
    new_shape: Shape,
    samples: u64,
}

fn parse_shadow_flags(args: &[String]) -> ShadowArgs {
    // A kevy paged reply is recognised from its shape. The ambiguity
    // that needs declaring is member/score pairs versus a plain list,
    // and only on the old side in practice.
    let mut a = ShadowArgs {
        host: crate::DEFAULT_HOST.to_string(),
        port: crate::DEFAULT_PORT,
        old: None,
        new: None,
        old_shape: Shape::Flat,
        new_shape: Shape::Paged,
        samples: 1,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" if i + 1 < args.len() => {
                a.host = args[i + 1].clone();
                i += 2;
            }
            "-p" if i + 1 < args.len() => {
                a.port = args[i + 1].parse().unwrap_or(crate::DEFAULT_PORT);
                i += 2;
            }
            "--old" if i + 1 < args.len() => {
                a.old = Some(args[i + 1].clone());
                i += 2;
            }
            "--new" if i + 1 < args.len() => {
                a.new = Some(args[i + 1].clone());
                i += 2;
            }
            "--old-pairs" => {
                a.old_shape = Shape::Pairs;
                i += 1;
            }
            "--new-flat" => {
                a.new_shape = Shape::Flat;
                i += 1;
            }
            "--samples" if i + 1 < args.len() => {
                a.samples = args[i + 1].parse().unwrap_or(1);
                i += 2;
            }
            _ => i += 1,
        }
    }
    a
}

/// `shadow [-h host] [-p port] --old "<cmd>" --new "<cmd>"
/// [--old-pairs] [--new-flat] [--samples n]`
///
/// Both sides are whole commands, quoted, because the old path is
/// whatever the application already runs — a ZRANGE, an LRANGE, a
/// SMEMBERS — and the new one is an `IDX.QUERY`. Nothing here knows
/// which; it compares the two orders of row keys they produce.
///
/// Exits non-zero on any divergence, so a cutover script can gate on
/// it without parsing the text.
pub fn run_shadow_cli(args: &[String]) -> ExitCode {
    let ShadowArgs { host, port, old, new, old_shape, new_shape, samples } =
        parse_shadow_flags(args);
    let (Some(old), Some(new)) = (old, new) else {
        eprintln!(
            "usage: kevy-cli shadow [-h host] [-p port] --old \"<command>\" \
             --new \"<command>\" [--old-pairs] [--new-flat] [--samples n]"
        );
        return ExitCode::FAILURE;
    };
    let split =
        |s: &str| -> Vec<Vec<u8>> { s.split_whitespace().map(|t| t.as_bytes().to_vec()).collect() };
    let mut client = match RespClient::connect(&host, port) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kevy-cli: could not connect to {host}:{port}: {e}");
            return ExitCode::FAILURE;
        }
    };
    match run(&mut client, &split(&old), &split(&new), old_shape, new_shape, samples) {
        Ok(report) => {
            print_report(&report);
            // A divergence is a finding, not a crash: exit non-zero so a
            // cutover script can gate on it without parsing the text.
            if report.diverged > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
        }
        Err(e) => {
            eprintln!("kevy-cli shadow: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(s: &str) -> Reply {
        Reply::Bulk(s.as_bytes().to_vec())
    }

    /// kevy's paged reply is recognised from its shape, so a caller who
    /// never thought about shapes still gets rows rather than nonsense.
    #[test]
    fn a_paged_reply_is_read_without_being_declared() {
        let reply = Reply::Array(vec![
            bulk("0"),
            Reply::Array(vec![bulk("u:1"), bulk("10"), bulk("u:2"), bulk("20")]),
        ]);
        let rows = rows_of(&reply, Shape::Flat); // deliberately the "wrong" shape
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], (b"u:1".to_vec(), b"10".to_vec()));
    }

    /// The ambiguity that cannot be detected: member/score pairs look
    /// exactly like a plain list. Reading WITHSCORES as flat would make
    /// every score a row key and report a divergence on every sample.
    #[test]
    fn pairs_and_flat_are_told_apart_by_the_caller() {
        let reply = Reply::Array(vec![bulk("u:1"), bulk("10"), bulk("u:2"), bulk("20")]);
        assert_eq!(rows_of(&reply, Shape::Flat).len(), 4, "flat: four rows");
        assert_eq!(rows_of(&reply, Shape::Pairs).len(), 2, "pairs: two rows with scores");
    }

    /// Lesson 2's consequence: a row the old path has and the new one
    /// does not is a writer nobody updated.
    #[test]
    fn a_row_only_the_old_path_has_is_reported_missing() {
        let old = vec![(b"u:1".to_vec(), vec![]), (b"u:2".to_vec(), vec![])];
        let new = vec![(b"u:1".to_vec(), vec![])];
        let c = compare(&old, &new);
        assert_eq!(c.missing, vec![b"u:2".to_vec()]);
        assert!(c.extra.is_empty());
    }

    /// Lesson 4's whole point: identical membership, different order.
    /// Set comparison alone calls this a match.
    #[test]
    fn identical_sets_in_different_orders_still_diverge() {
        let old = vec![(b"u:2".to_vec(), b"5".to_vec()), (b"u:1".to_vec(), b"10".to_vec())];
        let new = vec![(b"u:1".to_vec(), b"10".to_vec()), (b"u:2".to_vec(), b"20".to_vec())];
        let c = compare(&old, &new);
        assert!(c.missing.is_empty() && c.extra.is_empty(), "same membership");
        let d = c.first.expect("order must still diverge");
        assert_eq!(d.at, 0);
        // Both sort keys travel with it — that pair is what names the
        // drifting writer.
        assert_eq!(d.old.unwrap().1, b"5".to_vec());
        assert_eq!(d.new.unwrap().1, b"10".to_vec());
    }

    #[test]
    fn agreement_reports_nothing() {
        let rows = vec![(b"u:1".to_vec(), b"10".to_vec())];
        let c = compare(&rows, &rows);
        assert!(c.missing.is_empty() && c.extra.is_empty() && c.first.is_none());
    }
}

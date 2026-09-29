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

use kevy_resp_client::Reply;

mod command;
pub(crate) use command::run_on;

/// One side's reading of a reply: the row keys in order, each with the
/// sort value it was ordered by (empty when the shape does not carry
/// one).
type Rows = Vec<(Vec<u8>, Vec<u8>)>;

/// How to read a reply into rows. Guessing is not free here — reading
/// `ZRANGE … WITHSCORES` as a plain list silently treats every score as
/// a row key and reports a divergence on every sample — so the two
/// ambiguous shapes are told apart by the caller, not by a heuristic.
///
/// ```
/// use kevy_cli::Reply;
/// use kevy_cli::shadow::{Shape, compare, rows_of};
/// let reply = Reply::Array(vec![Reply::Bulk(b"a".to_vec()), Reply::Bulk(b"1".to_vec())]);
/// // the same reply read two ways: two rows, or one row with its score
/// assert_eq!(rows_of(&reply, Shape::Flat).len(), 2);
/// assert_eq!(rows_of(&reply, Shape::Pairs), vec![(b"a".to_vec(), b"1".to_vec())]);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Shape {
    /// `[cursor, [key, sortval, key, sortval, …]]` — kevy's paged
    /// index reply. Detected, not declared: a two-element array whose
    /// second element is an array cannot be anything else here.
    ///
    /// ```
    /// use kevy_cli::Reply;
    /// use kevy_cli::shadow::{Shape, rows_of};
    /// // IDX.QUERY: a cursor, then key / sort value pairs
    /// let page = Reply::Array(vec![Reply::Bulk(b"0".to_vec()), Reply::Array(vec![Reply::Bulk(b"user:7".to_vec()), Reply::Bulk(b"42".to_vec())])]);
    /// assert_eq!(rows_of(&page, Shape::Paged), vec![(b"user:7".to_vec(), b"42".to_vec())]);
    /// ```
    Paged,
    /// `[a, b, c, …]` — every element is a row key.
    ///
    /// ```
    /// use kevy_cli::Reply;
    /// use kevy_cli::shadow::{Shape, rows_of};
    /// // SMEMBERS, LRANGE, ZRANGE without scores: every element is a key
    /// let rows = rows_of(&Reply::Array(vec![Reply::Bulk(b"a".to_vec()), Reply::Bulk(b"b".to_vec())]), Shape::Flat);
    /// assert_eq!(rows, vec![(b"a".to_vec(), vec![]), (b"b".to_vec(), vec![])]);
    /// ```
    Flat,
    /// `[member, score, member, score, …]` — `WITHSCORES` and friends.
    ///
    /// ```
    /// use kevy_cli::Reply;
    /// use kevy_cli::shadow::{Shape, rows_of};
    /// // ZRANGE … WITHSCORES
    /// let reply = Reply::Array(vec![Reply::Bulk(b"a".to_vec()), Reply::Bulk(b"1.5".to_vec())]);
    /// assert_eq!(rows_of(&reply, Shape::Pairs), vec![(b"a".to_vec(), b"1.5".to_vec())]);
    /// ```
    Pairs,
}

/// Read a reply into ordered rows under `shape`. `Paged` is recognised
/// from the reply itself, so passing `Flat` for a kevy index reply
/// still does the right thing rather than reporting nonsense.
///
/// ```
/// use kevy_cli::Reply;
/// use kevy_cli::shadow::{Shape, rows_of};
/// let page = Reply::Array(vec![Reply::Bulk(b"0".to_vec()), Reply::Array(vec![Reply::Bulk(b"k".to_vec()), Reply::Bulk(b"9".to_vec())])]);
/// // a paged reply is recognised even when the caller said Flat
/// assert_eq!(rows_of(&page, Shape::Flat), rows_of(&page, Shape::Paged));
/// assert!(rows_of(&Reply::Int(1), Shape::Flat).is_empty(), "not a list: no rows");
/// ```
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
///
/// ```
/// use kevy_cli::Reply;
/// use kevy_cli::shadow::{Shape, compare, rows_of};
/// let old = rows_of(&Reply::Array(vec![Reply::Bulk(b"a".to_vec()), Reply::Bulk(b"b".to_vec())]), Shape::Flat);
/// let new = rows_of(&Reply::Array(vec![Reply::Bulk(b"b".to_vec()), Reply::Bulk(b"a".to_vec())]), Shape::Flat);
/// let d = compare(&old, &new).first.expect("same rows, different order");
/// assert_eq!((d.at, d.old.map(|r| r.0), d.new.map(|r| r.0)), (0, Some(b"a".to_vec()), Some(b"b".to_vec())));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct Divergence {
    /// Position of the first place the two orders differ.
    ///
    /// ```
    /// use kevy_cli::Reply;
    /// use kevy_cli::shadow::{Shape, compare, rows_of};
    /// let old = rows_of(&Reply::Array(vec![Reply::Bulk(b"a".to_vec()), Reply::Bulk(b"b".to_vec())]), Shape::Flat);
    /// let new = rows_of(&Reply::Array(vec![Reply::Bulk(b"a".to_vec()), Reply::Bulk(b"c".to_vec())]), Shape::Flat);
    /// assert_eq!(compare(&old, &new).first.map(|d| d.at), Some(1));
    /// ```
    pub at: usize,
    /// The old side's row and the value it was ordered by.
    ///
    /// ```
    /// use kevy_cli::Reply;
    /// use kevy_cli::shadow::{Shape, compare, rows_of};
    /// let old = rows_of(&Reply::Array(vec![Reply::Bulk(b"a".to_vec()), Reply::Bulk(b"5".to_vec())]), Shape::Pairs);
    /// let new = rows_of(&Reply::Array(vec![]), Shape::Pairs);
    /// let d = compare(&old, &new).first.unwrap();
    /// assert_eq!(d.old, Some((b"a".to_vec(), b"5".to_vec())), "the row and its sort value");
    /// ```
    pub old: Option<(Vec<u8>, Vec<u8>)>,
    /// The new side's, at the same position.
    ///
    /// ```
    /// use kevy_cli::Reply;
    /// use kevy_cli::shadow::{Shape, compare, rows_of};
    /// let old = rows_of(&Reply::Array(vec![Reply::Bulk(b"a".to_vec())]), Shape::Flat);
    /// let new = rows_of(&Reply::Array(vec![Reply::Bulk(b"a".to_vec()), Reply::Bulk(b"b".to_vec())]), Shape::Flat);
    /// let d = compare(&old, &new).first.unwrap();
    /// assert_eq!(d.new, Some((b"b".to_vec(), vec![])));
    /// assert_eq!(d.old, None, "the old side ended here");
    /// ```
    pub new: Option<(Vec<u8>, Vec<u8>)>,
}

/// Rows the new side lacks, rows it invents, and the first ordering
/// difference. Membership and order are reported separately because
/// they fail for different reasons: a missing row is a writer nobody
/// updated, a reordering is score drift.
///
/// ```
/// use kevy_cli::Reply;
/// use kevy_cli::shadow::{Shape, compare, rows_of};
/// let old = rows_of(&Reply::Array(vec![Reply::Bulk(b"a".to_vec()), Reply::Bulk(b"b".to_vec())]), Shape::Flat);
/// let new = rows_of(&Reply::Array(vec![Reply::Bulk(b"a".to_vec()), Reply::Bulk(b"c".to_vec())]), Shape::Flat);
/// let c = compare(&old, &new);
/// assert_eq!((c.missing, c.extra), (vec![b"b".to_vec()], vec![b"c".to_vec()]));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct Compared {
    /// Rows the old path returns and the new one does not.
    ///
    /// ```
    /// use kevy_cli::Reply;
    /// use kevy_cli::shadow::{Shape, compare, rows_of};
    /// let old = rows_of(&Reply::Array(vec![Reply::Bulk(b"a".to_vec()), Reply::Bulk(b"b".to_vec())]), Shape::Flat);
    /// let new = rows_of(&Reply::Array(vec![Reply::Bulk(b"a".to_vec())]), Shape::Flat);
    /// assert_eq!(compare(&old, &new).missing, [b"b".to_vec()]);
    /// ```
    pub missing: Vec<Vec<u8>>,
    /// Rows the new path returns and the old one does not.
    ///
    /// ```
    /// use kevy_cli::Reply;
    /// use kevy_cli::shadow::{Shape, compare, rows_of};
    /// let old = rows_of(&Reply::Array(vec![Reply::Bulk(b"a".to_vec())]), Shape::Flat);
    /// let new = rows_of(&Reply::Array(vec![Reply::Bulk(b"a".to_vec()), Reply::Bulk(b"b".to_vec())]), Shape::Flat);
    /// assert_eq!(compare(&old, &new).extra, [b"b".to_vec()]);
    /// ```
    pub extra: Vec<Vec<u8>>,
    /// The first position where the two orders differ, if any.
    ///
    /// ```
    /// use kevy_cli::Reply;
    /// use kevy_cli::shadow::{Shape, compare, rows_of};
    /// let old = rows_of(&Reply::Array(vec![Reply::Bulk(b"a".to_vec()), Reply::Bulk(b"b".to_vec())]), Shape::Flat);
    /// let new = rows_of(&Reply::Array(vec![Reply::Bulk(b"b".to_vec()), Reply::Bulk(b"a".to_vec())]), Shape::Flat);
    /// let c = compare(&old, &new);
    /// assert!(c.missing.is_empty() && c.extra.is_empty(), "same membership");
    /// assert_eq!(c.first.map(|d| d.at), Some(0), "but not the same order");
    /// ```
    pub first: Option<Divergence>,
}

/// Compare two readings: what is missing, what is extra, and where the
/// orders first part company.
///
/// ```
/// use kevy_cli::Reply;
/// use kevy_cli::shadow::{Shape, compare, rows_of};
/// let old = rows_of(&Reply::Array(vec![Reply::Bulk(b"a".to_vec()), Reply::Bulk(b"b".to_vec())]), Shape::Flat);
/// let new = rows_of(&Reply::Array(vec![Reply::Bulk(b"a".to_vec()), Reply::Bulk(b"b".to_vec())]), Shape::Flat);
/// let c = compare(&old, &new);
/// assert!(c.missing.is_empty() && c.extra.is_empty() && c.first.is_none());
/// ```
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
///
/// ```
/// use kevy_cli::shadow::{Shape, print_report, run};
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// # client.request_borrowed(&[b"ZADD", b"feed", b"1", b"a", b"2", b"b", b"3", b"c"])?;
/// # client.request_borrowed(&[b"RPUSH", b"feed:new", b"a", b"c"])?;
/// # let argv = |s: &str| s.split(' ').map(|w| w.as_bytes().to_vec()).collect::<Vec<_>>();
/// let r = run(&mut client, &argv("ZRANGE feed 0 -1"), &argv("LRANGE feed:new 0 -1"),
///     Shape::Flat, Shape::Flat, 3)?;
/// print_report(&r);
/// assert!(r.diverged > 0);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct ShadowReport {
    /// How many times both sides were asked.
    ///
    /// ```
    /// use kevy_cli::shadow::{Shape, run};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # client.request_borrowed(&[b"ZADD", b"feed", b"1", b"a", b"2", b"b", b"3", b"c"])?;
    /// # client.request_borrowed(&[b"RPUSH", b"feed:new", b"a", b"c"])?;
    /// # let argv = |s: &str| s.split(' ').map(|w| w.as_bytes().to_vec()).collect::<Vec<_>>();
    /// let r = run(&mut client, &argv("ZRANGE feed 0 -1"), &argv("LRANGE feed:new 0 -1"),
    ///     Shape::Flat, Shape::Flat, 3)?;
    /// assert_eq!(r.samples, 3);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub samples: u64,
    /// How many of those disagreed in membership or order.
    ///
    /// ```
    /// use kevy_cli::shadow::{Shape, run};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # client.request_borrowed(&[b"ZADD", b"feed", b"1", b"a", b"2", b"b", b"3", b"c"])?;
    /// # client.request_borrowed(&[b"RPUSH", b"feed:new", b"a", b"c"])?;
    /// # let argv = |s: &str| s.split(' ').map(|w| w.as_bytes().to_vec()).collect::<Vec<_>>();
    /// let same = argv("ZRANGE feed 0 -1");
    /// let r = run(&mut client, &same, &same, Shape::Flat, Shape::Flat, 2)?;
    /// assert_eq!(r.diverged, 0, "a path agrees with itself");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub diverged: u64,
    /// The first sample that disagreed, and how.
    ///
    /// ```
    /// use kevy_cli::shadow::{Shape, run};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # client.request_borrowed(&[b"ZADD", b"feed", b"1", b"a", b"2", b"b", b"3", b"c"])?;
    /// # client.request_borrowed(&[b"RPUSH", b"feed:new", b"a", b"c"])?;
    /// # let argv = |s: &str| s.split(' ').map(|w| w.as_bytes().to_vec()).collect::<Vec<_>>();
    /// let r = run(&mut client, &argv("ZRANGE feed 0 -1"), &argv("LRANGE feed:new 0 -1"),
    ///     Shape::Flat, Shape::Flat, 3)?;
    /// let (sample, c) = r.first.expect("diverged");
    /// assert_eq!((sample, c.missing), (0, vec![b"b".to_vec()]));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub first: Option<(u64, Compared)>,
}

/// Run both commands `samples` times and compare each pair.
///
/// Both sides are issued on the same connection, back to back, so the
/// window between them is as small as this can make it. A row written
/// between the two reads shows up as a divergence, which is why a
/// single disagreement is a lead rather than a verdict — the report
/// carries the count so a rate can be read off it.
///
/// ```
/// use kevy_cli::shadow::{Shape, run};
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// # client.request_borrowed(&[b"ZADD", b"feed", b"1", b"a", b"2", b"b", b"3", b"c"])?;
/// # client.request_borrowed(&[b"RPUSH", b"feed:new", b"a", b"c"])?;
/// # let argv = |s: &str| s.split(' ').map(|w| w.as_bytes().to_vec()).collect::<Vec<_>>();
/// // the new path, once its writer is fixed
/// client.request_borrowed(&[b"RPUSH", b"feed:fixed", b"a", b"b", b"c"])?;
/// let r = run(&mut client, &argv("ZRANGE feed 0 -1"), &argv("LRANGE feed:fixed 0 -1"),
///     Shape::Flat, Shape::Flat, 5)?;
/// assert_eq!((r.samples, r.diverged), (5, 0));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn run(
    client: &mut dyn crate::link::Link,
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
///
/// ```
/// use kevy_cli::shadow::{Shape, print_report, run};
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// # client.request_borrowed(&[b"ZADD", b"feed", b"1", b"a", b"2", b"b", b"3", b"c"])?;
/// # client.request_borrowed(&[b"RPUSH", b"feed:new", b"a", b"c"])?;
/// # let argv = |s: &str| s.split(' ').map(|w| w.as_bytes().to_vec()).collect::<Vec<_>>();
/// let r = run(&mut client, &argv("ZRANGE feed 0 -1"), &argv("LRANGE feed:new 0 -1"),
///     Shape::Flat, Shape::Flat, 3)?;
/// // "shadow: 3 samples, 3 diverged (first at sample 0)", then
/// // "MISSING from the new path (1): b" and the order difference
/// print_report(&r);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
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

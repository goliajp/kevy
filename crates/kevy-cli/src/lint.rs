//! `lint` — the two questions worth asking about a shape, before and
//! after the table exists.
//!
//! Lessons 1 and 6 of the migration playbook. They are one deliverable
//! in the plan and two commands here, because they run at different
//! moments and answer differently.
//!
//! **`lint overlap`** is lesson 1, and it is not the check the plan
//! first described. That plan said to sample a candidate column and see
//! whether it is single-valued — but a hash field holds one value by
//! construction, so that check passes forever. The lesson says where
//! the answer really lives: *"the answer is usually in your
//! id-derivation or key-construction code, not in the row itself — a
//! thread can live in several mailboxes."* The symptom of that **is**
//! in the data, just not in the row: the same name appears under more
//! than one owner. So this reads the family of owner-keyed collections
//! and asks whether they intersect. They do ⇒ no column can carry that
//! dimension, and a membership row is the shape.
//!
//! **`lint columns`** is lesson 6, and it can only run **after** the
//! table is declared — it reads rows. Two columns whose values coincide
//! on nearly every row are one column copied to get a second sort
//! order; the answer is another ORDERPATH, which `IDX.ADVISE` names.
//!
//! The exit codes differ on purpose. Overlap is an **answer**: a column
//! cannot carry a multi-valued dimension, so a script should stop.
//! Coincidence is a **suspicion** — two columns may legitimately agree
//! — so it reports and exits zero.
//!
//! ```
//! use kevy_cli::lint::{column_pairs, overlap};
//! # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
//! let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
//! // a thread can live in several mailboxes: no `mailbox` column can hold that
//! client.request_borrowed(&[b"SADD", b"box:inbox", b"t1", b"t2"])?;
//! client.request_borrowed(&[b"SADD", b"box:work", b"t2", b"t3"])?;
//! assert_eq!(overlap(&mut client, "box:")?.shared, 1);
//!
//! // `updated` is `created` copied on two rows of three
//! for (id, created, updated) in [("1", "10", "10"), ("2", "20", "20"), ("3", "30", "31")] {
//!     client.request_borrowed(&[b"HSET", format!("post:{id}").as_bytes(),
//!         b"created", created.as_bytes(), b"updated", updated.as_bytes()])?;
//! }
//! let (_, found) = column_pairs(&mut client, "post:", 100, 60)?;
//! assert_eq!((found[0].a.as_str(), found[0].b.as_str()), ("created", "updated"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::BTreeMap;
use std::io;

use crate::link::Link;

mod command;
pub(crate) use command::run_on;

/// What the owner-keyed collections under a prefix look like together.
///
/// ```
/// use kevy_cli::lint::overlap;
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// client.request_borrowed(&[b"SADD", b"box:inbox", b"t1", b"t2"])?;
/// client.request_borrowed(&[b"SADD", b"box:work", b"t2", b"t3"])?;
/// let o = overlap(&mut client, "box:")?;
/// assert_eq!((o.owners, o.names, o.shared, o.skipped), (2, 3, 1, 0));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct Overlap {
    /// How many owner collections were read.
    ///
    /// ```
    /// use kevy_cli::lint::overlap;
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # client.request_borrowed(&[b"SADD", b"box:inbox", b"t1", b"t2"])?;
    /// # client.request_borrowed(&[b"SADD", b"box:work", b"t2", b"t3"])?;
    /// assert_eq!(overlap(&mut client, "box:")?.owners, 2);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub owners: usize,
    /// Distinct names across all of them.
    ///
    /// ```
    /// use kevy_cli::lint::overlap;
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # client.request_borrowed(&[b"SADD", b"box:inbox", b"t1", b"t2"])?;
    /// # client.request_borrowed(&[b"SADD", b"box:work", b"t2", b"t3"])?;
    /// assert_eq!(overlap(&mut client, "box:")?.names, 3, "t1, t2, t3");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub names: usize,
    /// Names that appear under more than one owner.
    ///
    /// ```
    /// use kevy_cli::lint::overlap;
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # client.request_borrowed(&[b"SADD", b"box:inbox", b"t1", b"t2"])?;
    /// # client.request_borrowed(&[b"SADD", b"box:work", b"t2", b"t3"])?;
    /// assert_eq!(overlap(&mut client, "box:")?.shared, 1, "only t2 has two owners");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub shared: usize,
    /// A few of them, with the owners they appear under.
    ///
    /// ```
    /// use kevy_cli::lint::overlap;
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # client.request_borrowed(&[b"SADD", b"box:inbox", b"t1", b"t2"])?;
    /// # client.request_borrowed(&[b"SADD", b"box:work", b"t2", b"t3"])?;
    /// let o = overlap(&mut client, "box:")?;
    /// let mut owners = o.examples[0].1.clone();
    /// owners.sort();
    /// assert_eq!((o.examples[0].0.as_str(), owners), ("t2", vec!["box:inbox".to_string(), "box:work".into()]));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub examples: Vec<(String, Vec<String>)>,
    /// Keys under the prefix that are not collections at all — a
    /// counter or a hash sitting beside the owner sets. Reported so a
    /// prefix that matched the wrong family is visible.
    ///
    /// ```
    /// use kevy_cli::lint::overlap;
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # client.request_borrowed(&[b"SADD", b"box:inbox", b"t1", b"t2"])?;
    /// # client.request_borrowed(&[b"SADD", b"box:work", b"t2", b"t3"])?;
    /// client.request_borrowed(&[b"SET", b"box:count", b"2"])?; // a sidecar, not an owner
    /// let o = overlap(&mut client, "box:")?;
    /// assert_eq!((o.owners, o.skipped), (2, 1));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub skipped: usize,
}

/// Read every collection under `prefix` and see whether they intersect.
///
/// ```
/// use kevy_cli::lint::overlap;
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// client.request_borrowed(&[b"SADD", b"box:a", b"t1"])?;
/// client.request_borrowed(&[b"ZADD", b"box:b", b"1", b"t2"])?;
/// assert_eq!(overlap(&mut client, "box:")?.shared, 0, "a column can carry this");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn overlap(client: &mut dyn Link, prefix: &str) -> io::Result<Overlap> {
    let keys = crate::collections::scan_prefix(client, prefix)?;
    let mut owners_of: BTreeMap<Vec<u8>, Vec<String>> = BTreeMap::new();
    let (mut owners, mut skipped) = (0usize, 0usize);
    for k in &keys {
        let owner = String::from_utf8_lossy(k).into_owned();
        // Discovered, not named: a sidecar under the same prefix is a
        // neighbour, not a failure — but it is counted and reported.
        let Some(ms) = crate::collections::members_if_collection(client, &owner)? else {
            skipped += 1;
            continue;
        };
        owners += 1;
        for m in ms {
            owners_of.entry(m).or_default().push(owner.clone());
        }
    }
    let mut o = tally(owners, &owners_of);
    o.skipped = skipped;
    Ok(o)
}

/// The question lesson 1 actually asks, as a function: does any name
/// appear under more than one owner?
fn tally(owners: usize, owners_of: &BTreeMap<Vec<u8>, Vec<String>>) -> Overlap {
    let shared: Vec<_> = owners_of.iter().filter(|(_, o)| o.len() > 1).collect();
    Overlap {
        owners,
        skipped: 0,
        names: owners_of.len(),
        shared: shared.len(),
        examples: shared
            .iter()
            .take(5)
            .map(|(n, o)| (String::from_utf8_lossy(n).into_owned(), (*o).clone()))
            .collect(),
    }
}

/// Two columns that agree on most of the rows they both appear in.
///
/// ```
/// use kevy_cli::lint::column_pairs;
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// # for (id, created, updated) in [("1", "10", "10"), ("2", "20", "20"), ("3", "30", "31")] {
/// #     client.request_borrowed(&[b"HSET", format!("post:{id}").as_bytes(),
/// #         b"created", created.as_bytes(), b"updated", updated.as_bytes()])?;
/// # }
/// let (_, found) = column_pairs(&mut client, "post:", 100, 60)?;
/// let c = &found[0];
/// println!("{} and {} agree on {}% ({}/{})", c.a, c.b, c.percent(), c.same, c.compared);
/// assert_eq!((c.same, c.compared), (2, 3));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct Coincidence {
    /// One column.
    ///
    /// ```
    /// use kevy_cli::lint::column_pairs;
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # for (id, created, updated) in [("1", "10", "10"), ("2", "20", "20"), ("3", "30", "31")] {
    /// #     client.request_borrowed(&[b"HSET", format!("post:{id}").as_bytes(),
    /// #         b"created", created.as_bytes(), b"updated", updated.as_bytes()])?;
    /// # }
    /// let (_, found) = column_pairs(&mut client, "post:", 100, 60)?;
    /// assert_eq!(found[0].a, "created");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub a: String,
    /// The other.
    ///
    /// ```
    /// use kevy_cli::lint::column_pairs;
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # for (id, created, updated) in [("1", "10", "10"), ("2", "20", "20"), ("3", "30", "31")] {
    /// #     client.request_borrowed(&[b"HSET", format!("post:{id}").as_bytes(),
    /// #         b"created", created.as_bytes(), b"updated", updated.as_bytes()])?;
    /// # }
    /// let (_, found) = column_pairs(&mut client, "post:", 100, 60)?;
    /// assert_eq!(found[0].b, "updated");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub b: String,
    /// Rows where both are present and equal.
    ///
    /// ```
    /// use kevy_cli::lint::column_pairs;
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # for (id, created, updated) in [("1", "10", "10"), ("2", "20", "20"), ("3", "30", "31")] {
    /// #     client.request_borrowed(&[b"HSET", format!("post:{id}").as_bytes(),
    /// #         b"created", created.as_bytes(), b"updated", updated.as_bytes()])?;
    /// # }
    /// let (_, found) = column_pairs(&mut client, "post:", 100, 60)?;
    /// assert_eq!(found[0].same, 2);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub same: usize,
    /// Rows where both are present.
    ///
    /// ```
    /// use kevy_cli::lint::column_pairs;
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # for (id, created, updated) in [("1", "10", "10"), ("2", "20", "20"), ("3", "30", "31")] {
    /// #     client.request_borrowed(&[b"HSET", format!("post:{id}").as_bytes(),
    /// #         b"created", created.as_bytes(), b"updated", updated.as_bytes()])?;
    /// # }
    /// let (_, found) = column_pairs(&mut client, "post:", 100, 60)?;
    /// assert_eq!(found[0].compared, 3);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub compared: usize,
}

impl Coincidence {
    /// How often they agreed, as a percentage of rows compared.
    ///
    /// ```
    /// use kevy_cli::lint::column_pairs;
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # for (id, created, updated) in [("1", "10", "10"), ("2", "20", "20"), ("3", "30", "31")] {
    /// #     client.request_borrowed(&[b"HSET", format!("post:{id}").as_bytes(),
    /// #         b"created", created.as_bytes(), b"updated", updated.as_bytes()])?;
    /// # }
    /// let (_, found) = column_pairs(&mut client, "post:", 100, 60)?;
    /// assert_eq!(found[0].percent(), 66, "2 of 3, rounded down");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn percent(&self) -> u32 {
        (self.same * 100).checked_div(self.compared).unwrap_or(0) as u32
    }
}

/// Sample rows under a prefix and find column pairs that nearly always
/// carry the same value.
///
/// ```
/// use kevy_cli::lint::column_pairs;
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// # for (id, created, updated) in [("1", "10", "10"), ("2", "20", "20"), ("3", "30", "31")] {
/// #     client.request_borrowed(&[b"HSET", format!("post:{id}").as_bytes(),
/// #         b"created", created.as_bytes(), b"updated", updated.as_bytes()])?;
/// # }
/// let (sampled, found) = column_pairs(&mut client, "post:", 100, 60)?;
/// assert_eq!((sampled, found.len()), (3, 1));
/// // raise the bar and the pair no longer qualifies
/// assert!(column_pairs(&mut client, "post:", 100, 90)?.1.is_empty());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn column_pairs(
    client: &mut dyn Link,
    prefix: &str,
    sample: usize,
    threshold: u32,
) -> io::Result<(usize, Vec<Coincidence>)> {
    let keys = crate::collections::scan_prefix(client, prefix)?;
    let mut rows = Vec::new();
    for k in keys.iter().take(sample) {
        let row = hgetall(client, k)?;
        if !row.is_empty() {
            rows.push(row);
        }
    }
    Ok((rows.len(), coincidences(&rows, threshold)))
}

/// Column pairs that agree on at least `threshold` percent of the rows
/// where both are present, worst agreement last.
fn coincidences(rows: &[BTreeMap<String, Vec<u8>>], threshold: u32) -> Vec<Coincidence> {
    let mut pairs: BTreeMap<(String, String), (usize, usize)> = BTreeMap::new();
    for row in rows {
        let cols: Vec<&String> = row.keys().collect();
        for (i, a) in cols.iter().enumerate() {
            for b in &cols[i + 1..] {
                let e = pairs.entry(((*a).clone(), (*b).clone())).or_insert((0, 0));
                e.1 += 1;
                if row[*a] == row[*b] {
                    e.0 += 1;
                }
            }
        }
    }
    let mut out: Vec<Coincidence> = pairs
        .into_iter()
        .map(|((a, b), (same, compared))| Coincidence { a, b, same, compared })
        .filter(|c| c.percent() >= threshold)
        .collect();
    out.sort_by(|x, y| y.percent().cmp(&x.percent()).then(x.a.cmp(&y.a)));
    out
}

fn hgetall(client: &mut dyn Link, key: &[u8]) -> io::Result<BTreeMap<String, Vec<u8>>> {
    let reply = client.request_borrowed(&[b"HGETALL", key])?;
    let flat = crate::collections::bulks(reply);
    Ok(flat
        .chunks(2)
        .filter(|c| c.len() == 2)
        .map(|c| (String::from_utf8_lossy(&c[0]).into_owned(), c[1].clone()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn under(pairs: &[(&str, &[&str])]) -> BTreeMap<Vec<u8>, Vec<String>> {
        let mut m: BTreeMap<Vec<u8>, Vec<String>> = BTreeMap::new();
        for (name, owners) in pairs {
            m.insert(name.as_bytes().to_vec(), owners.iter().map(|o| o.to_string()).collect());
        }
        m
    }

    fn row(fields: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
        fields.iter().map(|(k, v)| (k.to_string(), v.as_bytes().to_vec())).collect()
    }

    /// The mail system's case: a thread that lives in several
    /// mailboxes. One name under two owners is the whole answer.
    #[test]
    fn a_name_under_two_owners_is_the_multi_valued_signal() {
        let o = tally(2, &under(&[("t1", &["m:1"]), ("t2", &["m:1", "m:2"]), ("t3", &["m:2"])]));
        assert_eq!((o.names, o.shared), (3, 1));
        assert_eq!(o.examples[0].0, "t2");
        assert_eq!(o.examples[0].1, ["m:1", "m:2"]);
    }

    /// Owners that share nothing mean a column *can* carry the
    /// dimension — the answer this check exists to give when it is yes.
    #[test]
    fn disjoint_owners_leave_nothing_shared() {
        let o = tally(2, &under(&[("x", &["a"]), ("y", &["b"])]));
        assert_eq!(o.shared, 0);
    }

    /// Lesson 6's shape: one column copied to get a second sort order.
    /// Drift in a few rows must not hide it, so the threshold is a
    /// percentage rather than "always equal".
    #[test]
    fn a_copied_column_shows_up_below_perfect_agreement() {
        let mut rows: Vec<_> =
            (0..9).map(|i| row(&[("a", "1"), ("b", "1"), ("c", &format!("{i}"))])).collect();
        rows.push(row(&[("a", "1"), ("b", "2"), ("c", "9")]));
        let found = coincidences(&rows, 90);
        let named: Vec<&str> = found.iter().map(|c| c.a.as_str()).collect();
        assert_eq!(found.len(), 1, "only a/b agree enough, got {named:?}");
        assert_eq!((found[0].a.as_str(), found[0].b.as_str()), ("a", "b"));
        assert_eq!(found[0].percent(), 90);
    }

    /// Above what was actually found, nothing is reported — the
    /// threshold is the caller's, not a fixed opinion.
    #[test]
    fn a_threshold_above_the_agreement_reports_nothing() {
        let rows = vec![row(&[("a", "1"), ("b", "1")]), row(&[("a", "1"), ("b", "2")])];
        assert!(coincidences(&rows, 60).is_empty());
        assert_eq!(coincidences(&rows, 50).len(), 1);
    }
}

//! `backfill-keys` — the union of every structure that can name an item.
//!
//! Lesson 3 of the migration playbook, and only the half a machine can
//! do. The lesson splits itself: *"build the backfill key-set from the
//! **union** of every structure that can name an item (old indexes, the
//! primary keyspace scan, archives), then write rows from the
//! authoritative record."* The union is mechanical. What the
//! authoritative record is, and what a row looks like, is knowledge
//! that lives in the application — a tool that guessed would write the
//! wrong rows confidently.
//!
//! So this command produces the key-set and nothing else, and it splits
//! its output the way a pipeline needs: **the names go to stdout**, one
//! per line, ready to feed whatever writes the rows; **the accounting
//! goes to stderr**, so redirecting the list does not lose it.
//!
//! The accounting is the point of doing this at all. Each source
//! reports how many names *only it* contributed — and every non-zero
//! number there is a row that backfilling from any single source would
//! have missed. That is the 89 % / 76 % drift the lesson was paid for,
//! measured on your own data instead of quoted from someone else's.
//!
//! ```
//! use kevy_cli::backfill_keys::{Source, collect};
//! # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
//! let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
//! // The index lost item 3; the keyspace still has it.
//! client.request_borrowed(&[b"ZADD", b"idx:mail-by-date", b"1", b"1", b"2", b"2"])?;
//! for id in ["1", "2", "3"] {
//!     client.request_borrowed(&[b"SET", format!("mail:{id}").as_bytes(), b"body"])?;
//! }
//! let sources = [
//!     Source::Index("idx:mail-by-date".into()),
//!     Source::Prefix { prefix: "mail:".into(), keep: false },
//! ];
//! let union = collect(&mut client, &sources)?;
//! assert!(union.names.contains(&b"3".to_vec()), "the union has what the index lost");
//! assert_eq!(union.sources[1].unique, 1, "only the keyspace named item 3");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

// The progress report goes to stderr, and a stderr that has gone away
// (a closed pipe, `| head`) is not a reason to abandon a backfill that
// is otherwise succeeding. What matters is written to the store.
#![expect(clippy::let_underscore_must_use, reason = "a report nobody is reading is not a failure")]

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::process::ExitCode;

use crate::link::Link;

/// Where a set of item names comes from.
///
/// ```
/// use kevy_cli::backfill_keys::Source;
/// // `--from-index mail:by-date --from-prefix mail: --from-file archive.txt`
/// let sources = [
///     Source::Index("mail:by-date".into()),
///     Source::Prefix { prefix: "mail:".into(), keep: false },
///     Source::File("archive.txt".into()),
/// ];
/// let labels: Vec<String> = sources.iter().map(Source::label).collect();
/// assert_eq!(labels, ["index mail:by-date", "prefix mail:", "file archive.txt"]);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Source {
    /// The members of a set, sorted set, or list key.
    ///
    /// ```
    /// use kevy_cli::backfill_keys::{Source, collect};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// client.request_borrowed(&[b"SADD", b"tags:rust", b"a", b"b"])?;
    /// let u = collect(&mut client, &[Source::Index("tags:rust".into())])?;
    /// assert_eq!(u.names.len(), 2);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Index(String),
    /// Every key in the keyspace under a prefix.
    ///
    /// ```
    /// use kevy_cli::backfill_keys::{Source, collect};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// client.request_borrowed(&[b"SET", b"user:7", b"x"])?;
    /// let source = Source::Prefix { prefix: "user:".into(), keep: false };
    /// assert_eq!(collect(&mut client, &[source])?.names, vec![b"7".to_vec()]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Prefix {
        /// The prefix to scan.
        ///
        /// ```
        /// use kevy_cli::backfill_keys::Source;
        /// let s = Source::Prefix { prefix: "order:".into(), keep: false };
        /// assert_eq!(s.label(), "prefix order:");
        /// ```
        prefix: String,
        /// Keep the whole key rather than stripping the prefix.
        ///
        /// Stripping is the default because the names then line up with
        /// the members of an index: `mail:123` under `mail:` becomes
        /// `123`, which is what a sorted set of ids holds. Keeping the
        /// prefix is right when the key *is* the name.
        ///
        /// ```
        /// use kevy_cli::backfill_keys::{Source, collect};
        /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
        /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
        /// client.request_borrowed(&[b"SET", b"mail:123", b"x"])?;
        /// let stripped = Source::Prefix { prefix: "mail:".into(), keep: false };
        /// let whole = Source::Prefix { prefix: "mail:".into(), keep: true };
        /// assert_eq!(collect(&mut client, &[stripped])?.names, vec![b"123".to_vec()]);
        /// assert_eq!(collect(&mut client, &[whole])?.names, vec![b"mail:123".to_vec()]);
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        keep: bool,
    },
    /// One name per line, from a file (an archive listing, an export).
    ///
    /// ```
    /// use kevy_cli::backfill_keys::{Source, collect};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// let dir = kevy_tmpdir::TmpDir::new("backfill-file");
    /// let listing = dir.path().join("archive.txt");
    /// std::fs::write(&listing, "41\n\n42\n")?; // blank lines are skipped
    /// let source = Source::File(listing.to_string_lossy().into_owned());
    /// let u = collect(&mut client, &[source])?;
    /// assert_eq!(u.names, vec![b"41".to_vec(), b"42".to_vec()]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    File(String),
}

impl Source {
    /// How this source prints in the report.
    ///
    /// ```
    /// use kevy_cli::backfill_keys::Source;
    /// assert_eq!(Source::Index("by-date".into()).label(), "index by-date");
    /// let whole = Source::Prefix { prefix: "mail:".into(), keep: true };
    /// assert_eq!(whole.label(), "prefix mail: (whole keys)");
    /// assert_eq!(Source::File("ids.txt".into()).label(), "file ids.txt");
    /// ```
    pub fn label(&self) -> String {
        match self {
            Source::Index(k) => format!("index {k}"),
            Source::Prefix { prefix, keep } => {
                format!("prefix {prefix}{}", if *keep { " (whole keys)" } else { "" })
            }
            Source::File(p) => format!("file {p}"),
        }
    }
}

/// What one source contributed.
///
/// ```
/// use kevy_cli::backfill_keys::{Source, collect};
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// client.request_borrowed(&[b"SADD", b"a", b"1", b"2"])?;
/// client.request_borrowed(&[b"SADD", b"b", b"2", b"3"])?;
/// let u = collect(&mut client, &[Source::Index("a".into()), Source::Index("b".into())])?;
/// let r = &u.sources[0];
/// assert_eq!((r.label.as_str(), r.total, r.unique), ("index a", 2, 1));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct SourceReport {
    /// How the source was named on the command line.
    ///
    /// ```
    /// use kevy_cli::backfill_keys::{Source, collect};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// client.request_borrowed(&[b"SADD", b"ids", b"1"])?;
    /// let u = collect(&mut client, &[Source::Index("ids".into())])?;
    /// assert_eq!(u.sources[0].label, "index ids");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub label: String,
    /// Names this source produced.
    ///
    /// ```
    /// use kevy_cli::backfill_keys::{Source, collect};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// client.request_borrowed(&[b"RPUSH", b"queue", b"x", b"y", b"z"])?;
    /// let u = collect(&mut client, &[Source::Index("queue".into())])?;
    /// assert_eq!(u.sources[0].total, 3);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub total: usize,
    /// Names **no other source** produced. Non-zero means backfilling
    /// from any single source would have missed these rows.
    ///
    /// ```
    /// use kevy_cli::backfill_keys::{Source, collect};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// client.request_borrowed(&[b"SADD", b"old", b"1", b"2"])?;
    /// client.request_borrowed(&[b"SADD", b"new", b"1", b"2", b"9"])?;
    /// let u = collect(&mut client, &[Source::Index("old".into()), Source::Index("new".into())])?;
    /// assert_eq!(u.sources[0].unique, 0, "the old index alone misses nothing");
    /// assert_eq!(u.sources[1].unique, 1, "only the new one names 9");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub unique: usize,
}

/// The union, and where each name came from.
///
/// ```
/// use kevy_cli::backfill_keys::{Source, collect};
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// client.request_borrowed(&[b"SADD", b"a", b"1"])?;
/// client.request_borrowed(&[b"SADD", b"b", b"1", b"2"])?;
/// let u = collect(&mut client, &[Source::Index("a".into()), Source::Index("b".into())])?;
/// assert_eq!((u.names.len(), u.sources.len()), (2, 2));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct Union {
    /// Every name, first-seen order, deduplicated.
    ///
    /// ```
    /// use kevy_cli::backfill_keys::{Source, collect};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// client.request_borrowed(&[b"RPUSH", b"first", b"b", b"a"])?;
    /// client.request_borrowed(&[b"RPUSH", b"second", b"a", b"c"])?;
    /// let sources = [Source::Index("first".into()), Source::Index("second".into())];
    /// let u = collect(&mut client, &sources)?;
    /// assert_eq!(u.names, vec![b"b".to_vec(), b"a".to_vec(), b"c".to_vec()]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub names: Vec<Vec<u8>>,
    /// One entry per source, in the order they were given.
    ///
    /// ```
    /// use kevy_cli::backfill_keys::{Source, collect};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// client.request_borrowed(&[b"SADD", b"x", b"1"])?;
    /// client.request_borrowed(&[b"SADD", b"y", b"1"])?;
    /// let u = collect(&mut client, &[Source::Index("y".into()), Source::Index("x".into())])?;
    /// let labels: Vec<_> = u.sources.iter().map(|s| s.label.as_str()).collect();
    /// assert_eq!(labels, ["index y", "index x"]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub sources: Vec<SourceReport>,
}

/// Read every source and union their names.
///
/// A source that cannot be read is an error, never an empty
/// contribution:
///
/// ```
/// use kevy_cli::backfill_keys::{Source, collect};
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// client.request_borrowed(&[b"ZADD", b"by-date", b"1", b"m1"])?;
/// let u = collect(&mut client, &[Source::Index("by-date".into())])?;
/// assert_eq!(u.names, vec![b"m1".to_vec()]);
///
/// let err = collect(&mut client, &[Source::Index("no-such-index".into())]).unwrap_err();
/// assert!(err.to_string().contains("does not exist"));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn collect(client: &mut dyn Link, sources: &[Source]) -> io::Result<Union> {
    let mut per_source: Vec<BTreeSet<Vec<u8>>> = Vec::with_capacity(sources.len());
    let mut names: Vec<Vec<u8>> = Vec::new();
    let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
    for s in sources {
        let got = read_source(client, s)?;
        for n in &got {
            if seen.insert(n.clone()) {
                names.push(n.clone());
            }
        }
        per_source.push(got.into_iter().collect());
    }
    let labels: Vec<String> = sources.iter().map(Source::label).collect();
    Ok(Union { names, sources: account(&labels, &per_source) })
}

/// Who contributed what. A name is *unique* to a source when no other
/// source produced it — which is the only number here worth reading,
/// because each one is a row that backfilling from a single source
/// would have missed.
fn account(labels: &[String], per_source: &[BTreeSet<Vec<u8>>]) -> Vec<SourceReport> {
    labels
        .iter()
        .zip(per_source)
        .enumerate()
        .map(|(i, (label, mine))| SourceReport {
            label: label.clone(),
            total: mine.len(),
            unique: mine
                .iter()
                .filter(|n| !per_source.iter().enumerate().any(|(j, o)| j != i && o.contains(*n)))
                .count(),
        })
        .collect()
}

fn read_source(client: &mut dyn Link, s: &Source) -> io::Result<Vec<Vec<u8>>> {
    match s {
        Source::Index(key) => crate::collections::members(client, key),
        Source::Prefix { prefix, keep } => read_prefix(client, prefix, *keep),
        Source::File(path) => Ok(std::fs::read_to_string(path)?
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| l.as_bytes().to_vec())
            .collect()),
    }
}

/// Every key under a prefix, stripped unless the caller wants the key
/// itself: stripped names line up with the members of an index, which
/// is what makes the union meaningful.
fn read_prefix(client: &mut dyn Link, prefix: &str, keep: bool) -> io::Result<Vec<Vec<u8>>> {
    Ok(crate::collections::scan_prefix(client, prefix)?
        .into_iter()
        .map(|k| if keep { k } else { k[prefix.len().min(k.len())..].to_vec() })
        .collect())
}

/// The accounting, on stderr so redirecting the names keeps it.
///
/// ```
/// use kevy_cli::backfill_keys::{Source, collect, print_report};
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// client.request_borrowed(&[b"SADD", b"ids", b"1", b"2"])?;
/// let u = collect(&mut client, &[Source::Index("ids".into())])?;
/// for name in &u.names {
///     println!("{}", String::from_utf8_lossy(name)); // stdout: the key-set
/// }
/// print_report(&u); // stderr: "2 name(s) in the union", then one line per source
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn print_report(u: &Union) {
    let e = io::stderr();
    let mut e = e.lock();
    let _ = writeln!(e, "{} name(s) in the union", u.names.len());
    for s in &u.sources {
        let _ = writeln!(e, "  {:<32} {} name(s), {} only here", s.label, s.total, s.unique);
    }
    let missed: usize = u.sources.iter().map(|s| s.unique).sum();
    let _ = if missed == 0 && u.sources.len() > 1 {
        writeln!(e, "every source named the same items — no source alone would have missed a row")
    } else if u.sources.len() > 1 {
        writeln!(
            e,
            "{missed} name(s) appear in only one source — backfilling from any single one \
             would have missed them"
        )
    } else {
        writeln!(e, "one source given; there is nothing to union it against")
    };
}

/// The sources, in the order given.
fn parse_args(args: &[String]) -> Result<Vec<Source>, String> {
    let (mut sources, mut keep) = (Vec::new(), false);
    let mut scan = crate::tools::argscan::Scan::new(args);
    while let Some(word) = scan.next() {
        match word {
            "--keep-prefix" => keep = true,
            "--from-index" => sources.push(Source::Index(scan.value(word)?.to_string())),
            "--from-prefix" => {
                sources.push(Source::Prefix { prefix: scan.value(word)?.to_string(), keep: false })
            }
            "--from-file" => sources.push(Source::File(scan.value(word)?.to_string())),
            other => return Err(crate::tools::argscan::unexpected(other)),
        }
    }
    if sources.is_empty() {
        return Err("give at least one source".into());
    }
    if keep {
        for s in &mut sources {
            if let Source::Prefix { keep: k, .. } = s {
                *k = true;
            }
        }
    }
    Ok(sources)
}

/// `backfill-keys --from-index K --from-prefix P [--keep-prefix]
/// --from-file F …` on `client`. Names to stdout, accounting to stderr. A
/// source that cannot be read is an error rather than an empty
/// contribution — a silently empty source is exactly the hole this command
/// exists to close.
pub(crate) fn run_on(client: &mut dyn Link, args: &[String]) -> ExitCode {
    let sources = match parse_args(args) {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("kevy-cli backfill-keys: {msg}");
            eprintln!(
                "usage: kevy-cli --kevy backfill-keys [--from-index <key>] \
                 [--from-prefix <p> [--keep-prefix]] [--from-file <path>] …"
            );
            return ExitCode::FAILURE;
        }
    };
    let u = match collect(client, &sources) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("kevy-cli backfill-keys: {e}");
            return ExitCode::FAILURE;
        }
    };
    let out = io::stdout();
    let mut out = out.lock();
    for n in &u.names {
        let _ = out.write_all(n);
        let _ = out.write_all(b"\n");
    }
    let _ = out.flush();
    print_report(&u);
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> BTreeSet<Vec<u8>> {
        names.iter().map(|n| n.as_bytes().to_vec()).collect()
    }

    fn labels(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("s{i}")).collect()
    }

    /// A name is unique to a source when no *other* source has it —
    /// the number that says "backfilling from this one alone would
    /// have missed these".
    #[test]
    fn unique_means_no_other_source_named_it() {
        let sources =
            [set(&["1", "2", "3"]), set(&["3", "4", "7"]), set(&["1", "2", "3", "4", "5"])];
        let r = account(&labels(3), &sources);
        assert_eq!((r[0].total, r[0].unique), (3, 0), "all of s0 is covered elsewhere");
        assert_eq!((r[1].total, r[1].unique), (3, 1), "only s1 names 7");
        assert_eq!((r[2].total, r[2].unique), (5, 1), "only s2 names 5");
    }

    /// Sources that agree contribute nothing unique — which is the
    /// answer that means the drift this lesson warns about is absent.
    #[test]
    fn sources_that_agree_have_nothing_unique() {
        let sources = [set(&["a", "b"]), set(&["b", "a"])];
        for r in account(&labels(2), &sources) {
            assert_eq!(r.unique, 0);
        }
    }

    /// With one source there is nothing to be unique against, so every
    /// name is — the report says so in words rather than letting the
    /// number read as drift.
    #[test]
    fn a_lone_source_owns_everything_it_names() {
        let r = account(&labels(1), &[set(&["a", "b"])]);
        assert_eq!((r[0].total, r[0].unique), (2, 2));
    }
}

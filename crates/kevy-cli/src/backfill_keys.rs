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

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::process::ExitCode;

use kevy_resp_client::RespClient;

/// Where a set of item names comes from.
pub enum Source {
    /// The members of a set, sorted set, or list key.
    Index(String),
    /// Every key in the keyspace under a prefix.
    Prefix {
        /// The prefix to scan.
        prefix: String,
        /// Keep the whole key rather than stripping the prefix.
        ///
        /// Stripping is the default because the names then line up with
        /// the members of an index: `mail:123` under `mail:` becomes
        /// `123`, which is what a sorted set of ids holds. Keeping the
        /// prefix is right when the key *is* the name.
        keep: bool,
    },
    /// One name per line, from a file (an archive listing, an export).
    File(String),
}

impl Source {
    /// How this source prints in the report.
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
pub struct SourceReport {
    /// How the source was named on the command line.
    pub label: String,
    /// Names this source produced.
    pub total: usize,
    /// Names **no other source** produced. Non-zero means backfilling
    /// from any single source would have missed these rows.
    pub unique: usize,
}

/// The union, and where each name came from.
pub struct Union {
    /// Every name, first-seen order, deduplicated.
    pub names: Vec<Vec<u8>>,
    /// One entry per source, in the order they were given.
    pub sources: Vec<SourceReport>,
}

/// Read every source and union their names.
pub fn collect(client: &mut RespClient, sources: &[Source]) -> io::Result<Union> {
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

fn read_source(client: &mut RespClient, s: &Source) -> io::Result<Vec<Vec<u8>>> {
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
fn read_prefix(client: &mut RespClient, prefix: &str, keep: bool) -> io::Result<Vec<Vec<u8>>> {
    Ok(crate::collections::scan_prefix(client, prefix)?
        .into_iter()
        .map(|k| if keep { k } else { k[prefix.len().min(k.len())..].to_vec() })
        .collect())
}

/// The accounting, on stderr so redirecting the names keeps it.
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

/// The command line: host, port, and the sources in the order given.
fn parse_args(args: &[String]) -> (String, u16, Vec<Source>) {
    let (mut host, mut port) = (crate::DEFAULT_HOST.to_string(), crate::DEFAULT_PORT);
    let (mut sources, mut keep) = (Vec::new(), false);
    let mut i = 0;
    while i < args.len() {
        let has_val = i + 1 < args.len();
        match args[i].as_str() {
            "-h" if has_val => {
                host = args[i + 1].clone();
                i += 2;
            }
            "-p" if has_val => {
                port = args[i + 1].parse().unwrap_or(crate::DEFAULT_PORT);
                i += 2;
            }
            "--keep-prefix" => {
                keep = true;
                i += 1;
            }
            "--from-index" if has_val => {
                sources.push(Source::Index(args[i + 1].clone()));
                i += 2;
            }
            "--from-prefix" if has_val => {
                sources.push(Source::Prefix { prefix: args[i + 1].clone(), keep: false });
                i += 2;
            }
            "--from-file" if has_val => {
                sources.push(Source::File(args[i + 1].clone()));
                i += 2;
            }
            _ => i += 1,
        }
    }
    if keep {
        for s in &mut sources {
            if let Source::Prefix { keep: k, .. } = s {
                *k = true;
            }
        }
    }
    (host, port, sources)
}

/// `backfill-keys [-h host] [-p port] --from-index K --from-prefix P
/// [--keep-prefix] --from-file F …`
pub fn run_backfill_keys_cli(args: &[String]) -> ExitCode {
    let (host, port, sources) = parse_args(args);
    if sources.is_empty() {
        eprintln!("kevy-cli backfill-keys: give at least one source");
        eprintln!(
            "usage: kevy-cli backfill-keys [-h host] [-p port] \
             [--from-index <key>] [--from-prefix <p> [--keep-prefix]] [--from-file <path>] …"
        );
        return ExitCode::FAILURE;
    }
    emit(&host, port, &sources)
}

/// Names to stdout, accounting to stderr. A source that cannot be read
/// is an error rather than an empty contribution — a silently empty
/// source is exactly the hole this command exists to close.
fn emit(host: &str, port: u16, sources: &[Source]) -> ExitCode {
    let mut client = match RespClient::connect(host, port) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kevy-cli backfill-keys: could not connect to {host}:{port}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let u = match collect(&mut client, sources) {
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
        let sources = [set(&["1", "2", "3"]), set(&["3", "4", "7"]), set(&["1", "2", "3", "4", "5"])];
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

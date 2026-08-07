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

use std::collections::BTreeMap;
use std::io;
use std::process::ExitCode;

use kevy_resp_client::{Reply, RespClient};

/// What the owner-keyed collections under a prefix look like together.
pub struct Overlap {
    /// How many owner collections were read.
    pub owners: usize,
    /// Distinct names across all of them.
    pub names: usize,
    /// Names that appear under more than one owner.
    pub shared: usize,
    /// A few of them, with the owners they appear under.
    pub examples: Vec<(String, Vec<String>)>,
    /// Keys under the prefix that are not collections at all — a
    /// counter or a hash sitting beside the owner sets. Reported so a
    /// prefix that matched the wrong family is visible.
    pub skipped: usize,
}

/// Read every collection under `prefix` and see whether they intersect.
pub fn overlap(client: &mut RespClient, prefix: &str) -> io::Result<Overlap> {
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
pub struct Coincidence {
    /// One column.
    pub a: String,
    /// The other.
    pub b: String,
    /// Rows where both are present and equal.
    pub same: usize,
    /// Rows where both are present.
    pub compared: usize,
}

impl Coincidence {
    /// How often they agreed, as a percentage of rows compared.
    pub fn percent(&self) -> u32 {
        (self.same * 100).checked_div(self.compared).unwrap_or(0) as u32
    }
}

/// Sample rows under a prefix and find column pairs that nearly always
/// carry the same value.
pub fn column_pairs(
    client: &mut RespClient,
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

fn hgetall(client: &mut RespClient, key: &[u8]) -> io::Result<BTreeMap<String, Vec<u8>>> {
    let reply = client.request_borrowed(&[b"HGETALL", key])?;
    let flat = crate::collections::bulks(reply);
    Ok(flat
        .chunks(2)
        .filter(|c| c.len() == 2)
        .map(|c| (String::from_utf8_lossy(&c[0]).into_owned(), c[1].clone()))
        .collect())
}

/// The declared prefix of a table, from `TABLE.LIST`.
fn table_prefix(client: &mut RespClient, table: &str) -> io::Result<String> {
    let Reply::Array(tables) = client.request_borrowed(&[b"TABLE.LIST"])? else {
        return Err(io::Error::other("TABLE.LIST did not answer with a list"));
    };
    for t in &tables {
        let Reply::Array(items) = t else { continue };
        let f = crate::doctor::fields(items);
        let named = f.iter().any(|(k, v)| k == "name" && v == table);
        if named && let Some((_, p)) = f.iter().find(|(k, _)| k == "prefix") {
            return Ok(p.clone());
        }
    }
    Err(io::Error::other(format!("no declared table named '{table}'")))
}

/// `lint overlap --prefix <p>` / `lint columns <table> [--sample N]
/// [--threshold PCT]`
pub fn run_lint_cli(args: &[String]) -> ExitCode {
    let (mut host, mut port) = (crate::DEFAULT_HOST.to_string(), crate::DEFAULT_PORT);
    let (mut prefix, mut table) = (String::new(), String::new());
    let (mut sample, mut threshold) = (1000usize, 90u32);
    let sub = args.first().cloned().unwrap_or_default();
    let mut i = 1;
    while i < args.len() {
        let val = args.get(i + 1);
        match (args[i].as_str(), val) {
            ("-h", Some(v)) => host = v.clone(),
            ("-p", Some(v)) => port = v.parse().unwrap_or(crate::DEFAULT_PORT),
            ("--prefix", Some(v)) => prefix = v.clone(),
            ("--sample", Some(v)) => sample = v.parse().unwrap_or(sample),
            ("--threshold", Some(v)) => threshold = v.parse().unwrap_or(threshold),
            (other, _) if !other.starts_with('-') && table.is_empty() => {
                table = other.to_string();
                i += 1;
                continue;
            }
            _ => {
                i += 1;
                continue;
            }
        }
        i += 2;
    }
    let mut client = match RespClient::connect(&host, port) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kevy-cli lint: could not connect to {host}:{port}: {e}");
            return ExitCode::FAILURE;
        }
    };
    match sub.as_str() {
        "overlap" => run_overlap(&mut client, &prefix),
        "columns" => run_columns(&mut client, &table, sample, threshold),
        other => {
            eprintln!("kevy-cli lint: unknown subcommand '{other}'");
            eprintln!("usage: kevy-cli lint overlap --prefix <p>");
            eprintln!("       kevy-cli lint columns <table> [--sample N] [--threshold PCT]");
            ExitCode::FAILURE
        }
    }
}

/// Overlap is an answer, not a hint: a column cannot carry a dimension
/// that names more than one owner, so a non-empty intersection exits
/// non-zero and a declaring script stops.
fn run_overlap(client: &mut RespClient, prefix: &str) -> ExitCode {
    if prefix.is_empty() {
        eprintln!("kevy-cli lint overlap: --prefix names the family of owner keys");
        return ExitCode::FAILURE;
    }
    let o = match overlap(client, prefix) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("kevy-cli lint overlap: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("{} owner(s) under {prefix}, {} distinct name(s)", o.owners, o.names);
    if o.skipped > 0 {
        println!("  ({} key(s) under this prefix are not collections and were skipped)", o.skipped);
    }
    if o.owners == 0 {
        println!("no collection under {prefix} — is that the right prefix?");
        return ExitCode::FAILURE;
    }
    if o.shared == 0 {
        println!("no name appears under more than one owner — a column can carry this dimension");
        return ExitCode::SUCCESS;
    }
    println!("{} name(s) appear under more than one owner:", o.shared);
    for (name, owners) in &o.examples {
        println!("  {name}  →  {}", owners.join(", "));
    }
    println!(
        "this dimension is multi-valued, so no column can hold it — model a membership row \
         per (owner, item) and let an ORDERPATH sort it"
    );
    ExitCode::FAILURE
}

/// Coincidence is a suspicion — two columns may legitimately agree —
/// so this reports and exits zero whatever it finds.
fn run_columns(client: &mut RespClient, table: &str, sample: usize, threshold: u32) -> ExitCode {
    if table.is_empty() {
        eprintln!("kevy-cli lint columns: name a declared table");
        return ExitCode::FAILURE;
    }
    let prefix = match table_prefix(client, table) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("kevy-cli lint columns: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (rows, found) = match column_pairs(client, &prefix, sample, threshold) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("kevy-cli lint columns: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("{table}: {rows} row(s) sampled under {prefix}");
    if found.is_empty() {
        println!("no two columns agree on {threshold}% or more of them");
        return ExitCode::SUCCESS;
    }
    for c in &found {
        println!("  {} and {} agree on {}% ({}/{})", c.a, c.b, c.percent(), c.same, c.compared);
    }
    println!(
        "a column copied to get a second sort order is the shape lesson 6 warns about — \
         the answer is another ORDERPATH; ask IDX.ADVISE which one"
    );
    ExitCode::SUCCESS
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

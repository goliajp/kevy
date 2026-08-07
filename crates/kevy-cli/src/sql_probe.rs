//! `sql probe` — replay the funcgate corpus and classify every record.
//!
//! Three verdicts per record (RFC 2026-08-08 §4):
//!   * PASS    — folded, and the output matches the probe expectation;
//!     `statement error` records pass when folding errors.
//!   * REFUSED — a named refusal (table statements, unknown function,
//!     unsupported cast…). Honest and countable.
//!   * WRONG   — folded WITHOUT error but the answer differs. A silent
//!     wrong answer is the one class the gate never tolerates:
//!     one WRONG fails funcgate regardless of every ratio.
//!
//! The function-subset files (the RFC's 37) additionally feed the
//! served-ratio bar; the rest are reported for the 100%-classified
//! line. Output is one row per file plus a machine-readable summary.

use std::process::ExitCode;

/// The RFC's function-subset file prefixes: 8 date/time + 2
/// format/regexp + the scalar block + the mysql/pg time aliases.
const SUBSET: &[&str] = &[
    "06", "07", "08", "09", "10", "11", "12", "13", "32", "33", "36", "37", "38", "39", "40",
    "41", "42", "43", "44", "45", "46", "47", "48", "49", "50", "51", "52", "53", "54", "55",
    "56", "57", "58", "62", "65", "66", "67",
];

/// The corpus clock (probe 08 header): 2025-06-15T12:00:00Z.
const CORPUS_NOW: i64 = 1_749_988_800 * 1_000_000;

struct Tally {
    pass: u32,
    refused: u32,
    wrong: u32,
    /// Refusals of SELECT-shaped records — the function face's own
    /// gaps, as opposed to table/DDL records the eval face can never
    /// serve. The bar ratio reads pass/(pass+refused_select+wrong).
    refused_select: u32,
}

/// One parsed record: the SQL text, and what the file expects back.
enum Expect {
    Rows(Vec<String>),
    StatementOk,
    StatementError,
}

pub(crate) fn run_probe(dir: &str) -> ExitCode {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".test"))
            .collect(),
        Err(e) => {
            eprintln!("kevy-cli sql probe: {dir}: {e}");
            return ExitCode::FAILURE;
        }
    };
    names.sort();
    let (mut sub_pass, mut sub_total, mut wrong_total) = (0u32, 0u32, 0u32);
    let mut sub_foldable = 0u32;
    for name in &names {
        let src = match std::fs::read_to_string(format!("{dir}/{name}")) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("kevy-cli sql probe: {name}: {e}");
                return ExitCode::FAILURE;
            }
        };
        let t = run_file(&src);
        let in_subset = SUBSET.iter().any(|p| name.starts_with(p));
        if in_subset {
            sub_pass += t.pass;
            sub_total += t.pass + t.refused + t.wrong;
            sub_foldable += t.pass + t.refused_select + t.wrong;
        }
        wrong_total += t.wrong;
        print_file_row(name, &t, in_subset);
    }
    let pct = |num: u32, den: u32| {
        if den == 0 { 0.0 } else { f64::from(num) / f64::from(den) * 100.0 }
    };
    println!(
        "probe-summary: files={} subset-served={sub_pass}/{sub_total} ({:.1}%) \
         subset-foldable={sub_pass}/{sub_foldable} ({:.1}%) wrong={wrong_total}",
        names.len(),
        pct(sub_pass, sub_total),
        pct(sub_pass, sub_foldable),
    );
    ExitCode::SUCCESS
}

fn print_file_row(name: &str, t: &Tally, in_subset: bool) {
    println!(
        "{name:<44} pass={:<3} refused={:<3} wrong={}{}",
        t.pass,
        t.refused,
        t.wrong,
        if in_subset { "  [subset]" } else { "" }
    );
}

fn run_file(src: &str) -> Tally {
    let mut t = Tally { pass: 0, refused: 0, wrong: 0, refused_select: 0 };
    for (sql, expect) in parse_records(src) {
        classify(&sql, &expect, &mut t);
    }
    t
}

fn classify(sql: &str, expect: &Expect, t: &mut Tally) {
    let foldable = sql.trim_start().to_ascii_lowercase().starts_with("select");
    if !foldable {
        // DDL/DML records need an engine; the eval face refuses them
        // by name (`statement error` on DDL still counts refused — we
        // cannot confirm the error is for the probe's reason).
        t.refused += 1;
        return;
    }
    match (kevy_sql::fold_select(sql, CORPUS_NOW), expect) {
        (Ok(f), Expect::Rows(want)) => {
            let got: Vec<String> = f.columns.iter().map(slt_render).collect();
            if &got == want {
                t.pass += 1;
            } else {
                t.wrong += 1;
            }
        }
        (Ok(_), Expect::StatementError) => t.wrong += 1,
        (Ok(_), Expect::StatementOk) => t.pass += 1,
        (Err(_), Expect::StatementError) => t.pass += 1,
        (Err(_), _) => {
            t.refused += 1;
            t.refused_select += 1;
        }
    }
}

/// The sqllogictest subset the corpus uses: `query <type> [nosort]` /
/// SQL lines / `----` / expected lines (to a blank line), and
/// `statement ok|error` / SQL lines (to a blank line). Comments and
/// blank lines between records are skipped.
fn parse_records(src: &str) -> Vec<(String, Expect)> {
    let mut out = Vec::new();
    let mut lines = src.lines().peekable();
    while let Some(line) = lines.next() {
        let head = line.trim();
        if head.starts_with("query") {
            let mut sql = String::new();
            for l in lines.by_ref() {
                if l.trim() == "----" {
                    break;
                }
                push_line(&mut sql, l);
            }
            let mut rows = Vec::new();
            while let Some(l) = lines.peek() {
                if l.trim().is_empty() {
                    break;
                }
                rows.push(lines.next().expect("peeked").trim_end().to_string());
            }
            out.push((sql, Expect::Rows(rows)));
        } else if head.starts_with("statement") {
            let expect =
                if head.contains("error") { Expect::StatementError } else { Expect::StatementOk };
            let mut sql = String::new();
            while let Some(l) = lines.peek() {
                if l.trim().is_empty() {
                    break;
                }
                push_line(&mut sql, lines.next().expect("peeked"));
            }
            out.push((sql, expect));
        }
        // Comment / blank / anything else: skip.
    }
    out
}

/// sqllogictest's value forms: booleans print 1/0 (probe 88's runner
/// convention), NULL prints `NULL`, the empty string prints `(empty)`.
fn slt_render(v: &kevy_sql::Scalar) -> String {
    match v {
        kevy_sql::Scalar::Null => "NULL".to_string(),
        kevy_sql::Scalar::Bool(b) => (if *b { "1" } else { "0" }).to_string(),
        other => {
            let s = other.render();
            if s.is_empty() { "(empty)".to_string() } else { s }
        }
    }
}

fn push_line(sql: &mut String, l: &str) {
    if !sql.is_empty() {
        sql.push('\n');
    }
    sql.push_str(l);
}

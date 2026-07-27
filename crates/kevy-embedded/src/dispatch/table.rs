//! TABLE.* verbs for the embedded RESP dispatch. The grammar, the
//! validation and the compile all live in `kevy-index` — the SAME
//! calls the server makes, so the two wire faces cannot drift; reply
//! shapes and error wording mirror `crates/kevy/src/cmd_table.rs`
//! byte-for-byte (the dispatch oracle compares them).

use super::util::{arr, bulk, err, int, kevy_err};
use crate::store::Store;

/// One TABLE request; `false` = verb not in this group (which the
/// caller renders as unknown-command — matching the server, where a
/// malformed arity falls off the Extension route).
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    match up {
        b"TABLE.DECLARE" => cmd_declare(s, argv, out),
        b"TABLE.ENSURE" => cmd_ensure(s, argv, out),
        b"TABLE.REPLACE" => cmd_replace(s, argv, out),
        b"TABLE.DROP" => {
            if argv.len() != 2 {
                err(out, "ERR usage: TABLE.DROP name");
            } else {
                int(out, i64::from(s.table_drop(&argv[1])));
            }
        }
        b"TABLE.LIST" => {
            if argv.len() != 1 {
                err(out, "ERR usage: TABLE.LIST");
            } else {
                cmd_list(s, out);
            }
        }
        b"TABLE.VERIFY" => {
            if argv.len() != 2 {
                err(out, "ERR usage: TABLE.VERIFY name");
            } else {
                cmd_verify(s, &argv[1], out);
            }
        }
        _ => return false,
    }
    true
}

/// `TABLE.DECLARE …` — the shared parse, then the Store capability
/// (dry-run + synchronous compiled-index builds).
fn cmd_declare(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let refs: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
    match kevy_index::parse_table_declare(&refs) {
        Err(e) => err(out, &e),
        Ok(spec) => match s.table_declare(spec) {
            Ok(()) => out.extend_from_slice(b"+OK\r\n"),
            Err(e) => kevy_err(out, &e),
        },
    }
}

/// `TABLE.ENSURE …` — the boot verb: identical spec answers
/// `+UNCHANGED`, a different one refuses by name (server parity).
fn cmd_ensure(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let refs: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
    match kevy_index::parse_table_declare(&refs) {
        Err(e) => err(out, &e),
        Ok(spec) => match s.table_ensure(spec) {
            Ok(kevy_index::TableEnsure::Created) => out.extend_from_slice(b"+OK\r\n"),
            Ok(kevy_index::TableEnsure::Unchanged) => out.extend_from_slice(b"+UNCHANGED\r\n"),
            Err(e) => kevy_err(out, &e),
        },
    }
}

/// `TABLE.REPLACE …` — drop + redeclare; a bad spec refuses before the
/// old table drops (server parity).
fn cmd_replace(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let refs: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
    match kevy_index::parse_table_declare(&refs) {
        Err(e) => err(out, &e),
        Ok(spec) => match s.table_replace(spec) {
            Ok(()) => out.extend_from_slice(b"+OK\r\n"),
            Err(e) => kevy_err(out, &e),
        },
    }
}

/// `TABLE.LIST` — 12-field rows matching the server's reduce.
fn cmd_list(s: &Store, out: &mut Vec<u8>) {
    let tables = s.table_list();
    arr(out, tables.len());
    for t in &tables {
        arr(out, 12);
        bulk(out, b"name");
        bulk(out, &t.name);
        bulk(out, b"prefix");
        bulk(out, &t.prefix);
        bulk(out, b"pk");
        bulk(out, &t.pk);
        bulk(out, b"columns");
        bulk(out, t.columns.len().to_string().as_bytes());
        bulk(out, b"indexes");
        bulk(out, t.indexes.len().to_string().as_bytes());
        bulk(out, b"orderpaths");
        bulk(out, t.orderpaths.len().to_string().as_bytes());
    }
}

/// `TABLE.VERIFY name` — per compiled index the IDX.VERIFY sextet led
/// by the index name, then the spot-check pair (the server's exact
/// reply shape; embedded builds are synchronous, so never BUILDING).
fn cmd_verify(s: &Store, name: &[u8], out: &mut Vec<u8>) {
    const LABELS: [&[u8]; 10] = [
        b"entries", b"bytes", b"coerce_failures", b"duplicates", b"drift", b"checked",
        b"excluded", b"absent", b"rows", b"missing",
    ];
    let Ok(report) = s.table_verify_report(name) else {
        let n = String::from_utf8_lossy(name);
        return err(out, &format!("ERR no such table '{n}' (TABLE.LIST enumerates them)"));
    };
    let spot = [report.spot_rows, report.spot_type_mismatches];
    let per_index: Vec<(Vec<u8>, [u64; 10])> = report
        .per_index
        .into_iter()
        .map(|i| {
            (i.name, [
                i.entries, i.approx_bytes, i.coerce_failures, i.duplicates, i.drift, i.checked,
                i.excluded, i.absent, i.rows, i.missing,
            ])
        })
        .collect();
    arr(out, per_index.len() + 1);
    for (iname, sums) in &per_index {
        arr(out, 22);
        bulk(out, b"index");
        bulk(out, iname);
        for (label, v) in LABELS.iter().zip(sums.iter()) {
            bulk(out, label);
            bulk(out, v.to_string().as_bytes());
        }
    }
    arr(out, 4);
    bulk(out, b"spotcheck_rows");
    bulk(out, spot[0].to_string().as_bytes());
    bulk(out, b"spotcheck_type_mismatches");
    bulk(out, spot[1].to_string().as_bytes());
}

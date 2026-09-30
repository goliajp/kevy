//! TABLE.* verbs for the embedded RESP dispatch. The grammar, the
//! validation and the compile all live in `kevy-index` — the SAME
//! calls the server makes, so the two wire faces cannot drift; reply
//! shapes and error wording mirror `crates/kevy/src/cmd_table.rs`
//! byte-for-byte (the dispatch oracle compares them).

use super::kevy_err;
use crate::store::Store;
use kevy_resp::{encode_array_len, encode_bulk, encode_error, encode_integer};

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
                encode_error(out, "ERR usage: TABLE.DROP name");
            } else {
                match s.table_drop(&argv[1]) {
                    Ok(hit) => encode_integer(out, i64::from(hit)),
                    Err(e) => kevy_err(out, &e),
                }
            }
        }
        b"TABLE.LIST" => {
            if argv.len() != 1 {
                encode_error(out, "ERR usage: TABLE.LIST");
            } else {
                cmd_list(s, out);
            }
        }
        b"TABLE.VERIFY" => {
            if argv.len() != 2 {
                encode_error(out, "ERR usage: TABLE.VERIFY name");
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
        Err(e) => encode_error(out, &e.to_wire()),
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
        Err(e) => encode_error(out, &e.to_wire()),
        Ok(spec) => match s.table_ensure(spec) {
            Ok(kevy_index::TableEnsure::Unchanged) => out.extend_from_slice(b"+UNCHANGED\r\n"),
            // created, or any later outcome that leaves the table as declared
            Ok(_) => out.extend_from_slice(b"+OK\r\n"),
            Err(e) => kevy_err(out, &e),
        },
    }
}

/// `TABLE.REPLACE …` — drop + redeclare; a bad spec refuses before the
/// old table drops (server parity).
fn cmd_replace(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let refs: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
    match kevy_index::parse_table_declare(&refs) {
        Err(e) => encode_error(out, &e.to_wire()),
        Ok(spec) => match s.table_replace(spec) {
            Ok(()) => out.extend_from_slice(b"+OK\r\n"),
            Err(e) => kevy_err(out, &e),
        },
    }
}

/// `TABLE.LIST` — 14-field rows matching the server's reduce.
fn cmd_list(s: &Store, out: &mut Vec<u8>) {
    let tables = s.table_list();
    encode_array_len(out, tables.len() as i64);
    for t in &tables {
        encode_array_len(out, 14);
        encode_bulk(out, b"name");
        encode_bulk(out, &t.name);
        encode_bulk(out, b"prefix");
        encode_bulk(out, &t.prefix);
        encode_bulk(out, b"pk");
        encode_bulk(out, &t.pk);
        encode_bulk(out, b"columns");
        encode_bulk(out, t.columns.len().to_string().as_bytes());
        encode_bulk(out, b"indexes");
        encode_bulk(out, t.indexes.len().to_string().as_bytes());
        encode_bulk(out, b"orderpaths");
        encode_bulk(out, t.orderpaths.len().to_string().as_bytes());
        encode_bulk(out, b"window");
        match &t.window {
            None => encode_bulk(out, b"-"),
            Some(w) => {
                let mut f = w.column.clone();
                f.extend_from_slice(format!(":{}:{}", w.span, w.bucket).as_bytes());
                encode_bulk(out, &f);
            }
        }
    }
}

/// `TABLE.VERIFY name` — per compiled index the IDX.VERIFY sextet led
/// by the index name, then the spot-check pair (the server's exact
/// reply shape; embedded builds are synchronous, so never BUILDING).
/// One index's ten counts, flattened into the order `LABELS` names them.
///
/// The array and the label list are two halves of one table and are zipped
/// together at the reply, so they must stay in the same order; keeping the
/// flattening here rather than inline in the reply loop is what makes that
/// pairing a single readable line.
fn index_counts(per_index: Vec<kevy_index::IndexVerify>) -> Vec<(Vec<u8>, [u64; 10])> {
    per_index
        .into_iter()
        .map(|i| {
            (
                i.name,
                [
                    i.entries,
                    i.approx_bytes,
                    i.coerce_failures,
                    i.duplicates,
                    i.drift,
                    i.checked,
                    i.excluded,
                    i.absent,
                    i.rows,
                    i.missing,
                ],
            )
        })
        .collect()
}

fn cmd_verify(s: &Store, name: &[u8], out: &mut Vec<u8>) {
    const LABELS: [&[u8]; 10] = [
        b"entries",
        b"bytes",
        b"coerce_failures",
        b"duplicates",
        b"drift",
        b"checked",
        b"excluded",
        b"absent",
        b"rows",
        b"missing",
    ];
    let Ok(report) = s.table_verify_report(name) else {
        let n = String::from_utf8_lossy(name);
        return encode_error(out, &format!("ERR no such table '{n}' (TABLE.LIST enumerates them)"));
    };
    let spot = [report.spot_rows, report.spot_type_mismatches];
    let per_index = index_counts(report.per_index);
    encode_array_len(out, (per_index.len() + 1) as i64);
    for (iname, sums) in &per_index {
        encode_array_len(out, 22);
        encode_bulk(out, b"index");
        encode_bulk(out, iname);
        for (label, v) in LABELS.iter().zip(sums.iter()) {
            encode_bulk(out, label);
            encode_bulk(out, v.to_string().as_bytes());
        }
    }
    encode_array_len(out, 4);
    encode_bulk(out, b"spotcheck_rows");
    encode_bulk(out, spot[0].to_string().as_bytes());
    encode_bulk(out, b"spotcheck_type_mismatches");
    encode_bulk(out, spot[1].to_string().as_bytes());
}

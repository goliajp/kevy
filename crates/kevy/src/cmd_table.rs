//! TABLE.* command surface.
//!
//! DECLARE/DROP are Local catalog mutations (sidecar-persisted, like
//! IDX.*/VIEW.*): the parse + compile both live in `kevy_index`
//! ([`kevy_index::parse_table_declare`] / [`kevy_index::compile_table`])
//! — ONE implementation the embedded dispatch calls too, so the two
//! wire faces cannot drift (the IDX.CREATE parity lesson).
//! LIST/VERIFY ride the extension fan-out beside VIEW.*.
//!
//! Law 3 holds: a table compiles at DECLARE time into explicitly-named
//! IDX access paths (`<table>.<col>`, `<table>.<orderpath>`); the
//! engine enforces no schema at query time and chooses no access path.

use std::path::Path;

use kevy_index::{Catalog, TableCatalog, TableSpec, compile_table, parse_table_declare, spec_diff};
use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer};
use kevy_rt::ExtensionReduced;
use kevy_store::Store;

use crate::cmd_index_query::{ST_BUILDING, ST_NOINDEX, ST_OK};
use crate::state::{CatalogState, Ctx, RuntimeState};

const SIDECAR: &str = "table-catalog.meta";

/// Rows the per-shard column spot check samples (bounded — VERIFY must
/// not become a full-table sweep of the row payloads).
const SPOTCHECK_ROWS: usize = 64;

/// Boot: load the persisted table catalog (after `cmd_index::boot` —
/// the compiled indexes live in the index catalog's own sidecar).
pub(crate) fn boot(state: &RuntimeState) {
    let Some(dir) = state.sidecar_dir() else { return };
    if let Ok(text) = std::fs::read_to_string(dir.join(SIDECAR))
        && let Some(cat) = TableCatalog::from_sidecar(&text)
        && !cat.is_empty()
    {
        state.install_table_catalog(cat);
    }
}

pub(crate) fn persist_sidecar(dir: Option<&Path>, cat: &TableCatalog) {
    let Some(dir) = dir else { return };
    let tmp = dir.join("table-catalog.meta.tmp");
    if std::fs::write(&tmp, cat.to_sidecar()).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join(SIDECAR));
    }
}

/// `TABLE.DECLARE <name> PREFIX <p> PK <col> COLUMN <n> <ty> …` — the
/// full grammar and every named refusal live in `kevy_index`. Atomic:
/// the table AND all its compiled indexes admit into cloned catalogs
/// first; nothing installs on any error.
pub(crate) fn cmd_table_declare<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    store: &kevy_store::Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    let argv: Vec<&[u8]> = (0..args.len()).map(|i| &args[i] as &[u8]).collect();
    let spec = match parse_table_declare(&argv) {
        Ok(s) => s,
        Err(e) => return encode_error(out, &e),
    };
    // The tiering floor discipline IDX.CREATE keeps (RFC §4 row 16):
    // compiled indexes are the fixed layer demotion cannot reclaim.
    if crate::cmd_index::tier_floor_refused(store, out) {
        return;
    }
    let mut tcat = ctx.state.catalogs.table().map(|c| (*c).clone()).unwrap_or_default();
    if let Err(e) = tcat.create(spec.clone()) {
        return encode_error(out, &e);
    }
    let mut icat: Catalog = ctx.state.catalogs.index().map(|c| (*c).clone()).unwrap_or_default();
    let compiled = match compile_table(&spec) {
        Ok(c) => c,
        Err(e) => return encode_error(out, &e),
    };
    for ispec in compiled {
        if let Err(e) = icat.create(ispec) {
            return encode_error(out, e);
        }
    }
    persist_sidecar(ctx.state.sidecar_dir(), &tcat);
    crate::cmd_index::persist_sidecar(ctx.state.sidecar_dir(), &icat);
    ctx.state.install_index_catalog(icat);
    ctx.state.install_table_catalog(tcat);
    out.extend_from_slice(b"+OK\r\n");
}

/// `TABLE.ENSURE …` — `TABLE.DECLARE`'s boot form (dogfood F8.2): the
/// same grammar, but an identical existing declaration is `+UNCHANGED`
/// instead of an error, and a *different* one is a named refusal
/// carrying which part differs. Never a silent rebuild — that cost has
/// its own verb ([`cmd_table_replace`]).
pub(crate) fn cmd_table_ensure<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    store: &kevy_store::Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    let argv: Vec<&[u8]> = (0..args.len()).map(|i| &args[i] as &[u8]).collect();
    let spec = match parse_table_declare(&argv) {
        Ok(s) => s,
        Err(e) => return encode_error(out, &e),
    };
    let existing = ctx.state.catalogs.table().and_then(|c| c.get(&spec.name).cloned());
    match existing {
        None => cmd_table_declare(ctx, store, args, out),
        Some(cur) if cur.sans_auto() == spec => out.extend_from_slice(b"+UNCHANGED\r\n"),
        Some(cur) => encode_error(out, &spec_diff(&cur.sans_auto(), &spec)),
    }
}

/// `TABLE.REPLACE …` — drop and redeclare, rebuilding every compiled
/// index from the rows. Named for its cost: a full backfill, asked for
/// explicitly. The new spec is validated *before* the old table drops,
/// so a bad replacement leaves the old one standing.
pub(crate) fn cmd_table_replace<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    store: &kevy_store::Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    let argv: Vec<&[u8]> = (0..args.len()).map(|i| &args[i] as &[u8]).collect();
    let spec = match parse_table_declare(&argv) {
        Ok(s) => s,
        Err(e) => return encode_error(out, &e),
    };
    if let Err(e) = compile_table(&spec) {
        return encode_error(out, &e);
    }
    let exists = ctx.state.catalogs.table().and_then(|c| c.get(&spec.name).cloned()).is_some();
    if exists {
        let mut scratch = Vec::new();
        cmd_table_drop_by_name(ctx, &spec.name, &mut scratch);
    }
    cmd_table_declare(ctx, store, args, out);
}

/// The drop body, callable with a bare name (REPLACE's first half).
fn cmd_table_drop_by_name(ctx: &Ctx<'_>, name: &[u8], out: &mut Vec<u8>) {
    let mut tcat = ctx.state.catalogs.table().map(|c| (*c).clone()).unwrap_or_default();
    let compiled: Vec<Vec<u8>> = tcat
        .get(name)
        .map(|s| {
            compile_table(s).map(|c| c.into_iter().map(|i| i.name).collect()).unwrap_or_default() // catalog entries were admitted validated
        })
        .unwrap_or_default();
    if tcat.drop_table(name) {
        let mut icat: Catalog =
            ctx.state.catalogs.index().map(|c| (*c).clone()).unwrap_or_default();
        for cname in &compiled {
            icat.drop_index(cname);
        }
        persist_sidecar(ctx.state.sidecar_dir(), &tcat);
        crate::cmd_index::persist_sidecar(ctx.state.sidecar_dir(), &icat);
        ctx.state.install_index_catalog(icat);
        ctx.state.install_table_catalog(tcat);
    }
    out.extend_from_slice(b"+OK\r\n");
}

/// `TABLE.DROP <name>` — drops the table AND its compiled indexes.
pub(crate) fn cmd_table_drop<A: ArgvView + ?Sized>(ctx: &Ctx<'_>, args: &A, out: &mut Vec<u8>) {
    if args.len() != 2 {
        return encode_error(out, "ERR usage: TABLE.DROP name");
    }
    let mut tcat = ctx.state.catalogs.table().map(|c| (*c).clone()).unwrap_or_default();
    let compiled: Vec<Vec<u8>> = tcat
        .get(&args[1])
        .map(|s| {
            compile_table(s).map(|c| c.into_iter().map(|i| i.name).collect()).unwrap_or_default() // catalog entries were admitted validated
        })
        .unwrap_or_default();
    let hit = tcat.drop_table(&args[1]);
    if hit {
        let mut icat: Catalog =
            ctx.state.catalogs.index().map(|c| (*c).clone()).unwrap_or_default();
        for name in &compiled {
            icat.drop_index(name);
        }
        persist_sidecar(ctx.state.sidecar_dir(), &tcat);
        crate::cmd_index::persist_sidecar(ctx.state.sidecar_dir(), &icat);
        ctx.state.install_index_catalog(icat);
        ctx.state.install_table_catalog(tcat);
    }
    encode_integer(out, i64::from(hit));
}

// ---------- extension fan-out (LIST / VERIFY) ----------

/// Per-shard half. LIST is catalog-only (the reduce renders it);
/// VERIFY re-runs every compiled index's drift recheck on this shard
/// plus a bounded column-type spot check over sampled rows.
pub(crate) fn extension_op(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let verb = argv.first().map(Vec::as_slice).unwrap_or(b"");
    if verb.eq_ignore_ascii_case(b"TABLE.LIST") {
        return vec![ST_OK];
    }
    if verb.eq_ignore_ascii_case(b"TABLE.VERIFY") {
        return op_verify(ctx, store, argv);
    }
    vec![ST_NOINDEX]
}

/// VERIFY chunk: `[ST_OK][n u32]` then per compiled index ten u64
/// (entries, bytes, coerce_failures, duplicates, drift, checked,
/// excluded, absent, rows, missing — every one fresh), then
/// two u64 (spotcheck rows, type mismatches).
fn op_verify(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some(spec) =
        argv.get(1).and_then(|n| ctx.state.catalogs.table().and_then(|c| c.get(n).cloned()))
    else {
        return vec![ST_NOINDEX];
    };
    let Ok(compiled) = compile_table(&spec) else {
        // Catalog entries were admitted validated; an Err here means
        // the sidecar was hand-edited — refuse rather than panic.
        return vec![ST_NOINDEX];
    };
    let mut chunk = vec![ST_OK];
    chunk.extend_from_slice(&(compiled.len() as u32).to_le_bytes());
    for ispec in &compiled {
        match crate::cmd_table_verify::index_verify_counts(ctx, store, &ispec.name) {
            Ok(counts) => {
                for v in counts {
                    chunk.extend_from_slice(&v.to_le_bytes());
                }
            }
            Err(e) if e.as_wire().starts_with("INDEXBUILDING") => return vec![ST_BUILDING],
            Err(_) => return vec![ST_NOINDEX],
        }
    }
    let (rows, mismatches) = spot_check(store, &spec);
    chunk.extend_from_slice(&rows.to_le_bytes());
    chunk.extend_from_slice(&mismatches.to_le_bytes());
    chunk
}

/// Sample up to [`SPOTCHECK_ROWS`] rows on this shard and check that
/// every PRESENT declared-typed column coerces (absent = NULL, never
/// an error — Law 3; a non-hash row under the prefix counts as a row
/// with no columns). All reads ride the no-promote peek.
fn spot_check(store: &mut Store, spec: &TableSpec) -> (u64, u64) {
    let mut pat = spec.prefix.clone();
    pat.push(b'*');
    let keys = store.collect_keys(Some(&pat), Some(SPOTCHECK_ROWS));
    let names: Vec<&[u8]> = spec.columns.iter().map(|(n, _)| n.as_slice()).collect();
    store.peek_scope(|s| {
        let (mut rows, mut mismatches) = (0u64, 0u64);
        for key in &keys {
            rows += 1;
            let Ok(Some(vals)) = s.peek_hash_fields(key, &names) else { continue };
            for ((_, ty), val) in spec.columns.iter().zip(&vals) {
                if let Some(raw) = val
                    && kevy_index::IndexValue::coerce(*ty, raw).is_none()
                {
                    mismatches += 1;
                }
            }
        }
        (rows, mismatches)
    })
}

// ---------- origin reduce ----------

/// Origin half for TABLE.LIST / TABLE.VERIFY.
pub(crate) fn extension_reduce(
    catalogs: &CatalogState,
    argv: &[Vec<u8>],
    chunks: Vec<Vec<u8>>,
) -> ExtensionReduced {
    let verb = argv.first().map(Vec::as_slice).unwrap_or(b"");
    if verb.eq_ignore_ascii_case(b"TABLE.LIST") {
        return ExtensionReduced::Reply(render_table_list(catalogs));
    }
    ExtensionReduced::Reply(reduce_verify(catalogs, argv, &chunks))
}

/// `TABLE.LIST` — 14-field rows, catalog order (the catalog is
/// process-global; shards contribute nothing).
fn render_table_list(catalogs: &CatalogState) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(cat) = catalogs.table() else {
        encode_array_len(&mut out, 0);
        return out;
    };
    encode_array_len(&mut out, cat.len() as i64);
    for s in cat.iter() {
        encode_array_len(&mut out, 14);
        encode_bulk(&mut out, b"name");
        encode_bulk(&mut out, &s.name);
        encode_bulk(&mut out, b"prefix");
        encode_bulk(&mut out, &s.prefix);
        encode_bulk(&mut out, b"pk");
        encode_bulk(&mut out, &s.pk);
        encode_bulk(&mut out, b"columns");
        encode_bulk(&mut out, s.columns.len().to_string().as_bytes());
        encode_bulk(&mut out, b"indexes");
        encode_bulk(&mut out, s.indexes.len().to_string().as_bytes());
        encode_bulk(&mut out, b"orderpaths");
        encode_bulk(&mut out, s.orderpaths.len().to_string().as_bytes());
        encode_bulk(&mut out, b"window");
        encode_bulk(&mut out, &window_field(s));
    }
    out
}

/// The LIST row's window cell: `<col>:<span>:<bucket>` or `-`.
fn window_field(s: &kevy_index::TableSpec) -> Vec<u8> {
    match &s.window {
        None => b"-".to_vec(),
        Some(w) => {
            let mut f = w.column.clone();
            f.extend_from_slice(format!(":{}:{}", w.span, w.bucket).as_bytes());
            f
        }
    }
}

/// `TABLE.VERIFY` reduce: sum the per-shard counters, render one
/// element per compiled index (the IDX.VERIFY label/value shape, led
/// by the index name) plus a trailing spot-check element.
fn reduce_verify(catalogs: &CatalogState, argv: &[Vec<u8>], chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let name_s = argv.get(1).map(|a| String::from_utf8_lossy(a).into_owned()).unwrap_or_default();
    let Some(spec) = argv.get(1).and_then(|n| catalogs.table().and_then(|c| c.get(n).cloned()))
    else {
        encode_error(
            &mut out,
            &format!("ERR no such table '{name_s}' (TABLE.LIST enumerates them)"),
        );
        return out;
    };
    let n = compile_table(&spec).map(|c| c.len()).unwrap_or_default();
    for c in chunks {
        match c.first().copied() {
            Some(x) if x == ST_OK => {}
            Some(x) if x == ST_BUILDING => {
                encode_error(
                    &mut out,
                    &format!(
                        "INDEXBUILDING table '{name_s}' has an index still building (poll IDX.LIST until state=ready)"
                    ),
                );
                return out;
            }
            _ => {
                encode_error(
                    &mut out,
                    &format!("ERR no such table '{name_s}' (TABLE.LIST enumerates them)"),
                );
                return out;
            }
        }
    }
    let (sums, spot) = fold_verify_chunks(n, chunks);
    render_verify(&mut out, &spec, &sums, spot);
    out
}

/// Sum per-index counter rows + the trailing spot-check pair across
/// shards.
fn fold_verify_chunks(n: usize, chunks: &[Vec<u8>]) -> (Vec<[u64; 10]>, [u64; 2]) {
    let mut sums = vec![[0u64; 10]; n];
    let mut spot = [0u64; 2];
    for c in chunks {
        let mut pos = 5usize; // status + n u32
        for s in sums.iter_mut() {
            for slot in s.iter_mut() {
                let Some(w) = c.get(pos..pos + 8) else { break };
                *slot += u64::from_le_bytes(w.try_into().expect("8 bytes"));
                pos += 8;
            }
        }
        for slot in &mut spot {
            let Some(w) = c.get(pos..pos + 8) else { break };
            *slot += u64::from_le_bytes(w.try_into().expect("8 bytes"));
            pos += 8;
        }
    }
    (sums, spot)
}

/// The reply body: per compiled index a 22-element label/value row
/// (ten fresh counters, led by the index name — the four newer
/// additions ride at the end so a label-reading 4.0 consumer keeps
/// working), then one 4-element spot-check row.
fn render_verify(out: &mut Vec<u8>, spec: &TableSpec, sums: &[[u64; 10]], spot: [u64; 2]) {
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
    encode_array_len(out, (sums.len() + 1) as i64);
    for (ispec, s) in compile_table(spec).unwrap_or_default().iter().zip(sums) {
        encode_array_len(out, 22);
        encode_bulk(out, b"index");
        encode_bulk(out, &ispec.name);
        for (label, v) in LABELS.iter().zip(s.iter()) {
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

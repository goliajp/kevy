//! TABLE.* command surface.
//!
//! DECLARE/DROP are Local catalog mutations (recorded like
//! IDX.*/VIEW.*): the parse + compile both live in `kevy_index`
//! ([`kevy_index::parse_table_declare`] / [`kevy_index::TableSpec::compile`])
//! — ONE implementation the embedded dispatch calls too, so the two
//! wire faces cannot drift (the IDX.CREATE parity lesson).
//! LIST/VERIFY ride the extension fan-out beside VIEW.*.
//!
//! Law 3 holds: a table compiles at DECLARE time into explicitly-named
//! IDX access paths (`<table>.<col>`, `<table>.<orderpath>`); the
//! engine enforces no schema at query time and chooses no access path.

use kevy_index::{
    Catalog, GlobalPath, TableCatalog, TableSpec, parse_table_declare_partitioned, spec_diff,
};
use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer};
use kevy_rt::ExtensionReduced;
use kevy_store::Store;

use crate::cmd_index_install::Sampler;
use crate::cmd_index_query::{ST_BUILDING, ST_NOINDEX, ST_OK};
use crate::state::{CatalogChange, CatalogState, Ctx};

/// Rows the per-shard column spot check samples (bounded — VERIFY must
/// not become a full-table sweep of the row payloads).
const SPOTCHECK_ROWS: usize = 64;

/// `TABLE.DECLARE` / `ENSURE` / `REPLACE` dispatched on the shard that
/// runs them: a sampled `GLOBAL` path samples this shard's rows (the
/// router sends such a declaration through the two-phase form, so this is
/// the path inside a transaction).
pub(crate) fn cmd_table_local<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    upper: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    let sampler = &mut Sampler::Shard(store);
    match upper {
        b"TABLE.DECLARE" => cmd_table_declare(ctx, sampler, args, out),
        b"TABLE.ENSURE" => cmd_table_ensure(ctx, sampler, args, out),
        _ => cmd_table_replace(ctx, sampler, args, out),
    }
}

/// `TABLE.DECLARE <name> PREFIX <p> PK <col> COLUMN <n> <ty> …` — the
/// full grammar and every named refusal live in `kevy_index`. Atomic:
/// the table AND all its compiled indexes admit into cloned catalogs
/// first; nothing installs on any error.
pub(crate) fn cmd_table_declare<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    sampler: &mut Sampler<'_>,
    args: &A,
    out: &mut Vec<u8>,
) {
    let argv: Vec<&[u8]> = (0..args.len()).map(|i| &args[i] as &[u8]).collect();
    let (spec, globals) = match parse_table_declare_partitioned(&argv) {
        Ok(s) => s,
        Err(e) => return encode_error(out, &e.to_wire()),
    };
    // The tiering floor discipline IDX.CREATE keeps (RFC §4 row 16):
    // compiled indexes are the fixed layer demotion cannot reclaim.
    if sampler.tier_blocked() {
        return encode_error(out, crate::cmd_index::TIER_FLOOR_REFUSAL);
    }
    let n = ctx.state.nshards();
    match change_tables(ctx, |t, i| declared_onto(t, i, &spec, &globals, sampler, n)) {
        Ok(_) => out.extend_from_slice(b"+OK\r\n"),
        Err(e) => encode_error(out, &e),
    }
}

/// Admit `spec` and the indexes it compiles into the table and index
/// catalogs `tcat` and `icat`.
fn declared_onto(
    tcat: &mut TableCatalog,
    icat: &mut Catalog,
    spec: &TableSpec,
    globals: &[GlobalPath],
    sampler: &mut Sampler<'_>,
    nshards: usize,
) -> Result<bool, String> {
    tcat.create(spec.clone()).map_err(|e| e.to_wire())?;
    let compiled = spec.compile().map_err(|e| e.to_wire())?;
    crate::cmd_table_global::admit(icat, compiled, globals, sampler, nshards)?;
    Ok(true)
}

/// Drop table `name` and the indexes it compiled from `tcat` and `icat`;
/// whether it was there.
fn dropped_from(tcat: &mut TableCatalog, icat: &mut Catalog, name: &[u8]) -> bool {
    let compiled: Vec<Vec<u8>> = tcat
        .get(name)
        .map(|s| {
            s.compile()
                .map(|c| c.into_iter().map(|i| i.name().to_vec()).collect())
                .unwrap_or_default() // catalog entries were admitted validated
        })
        .unwrap_or_default();
    if !tcat.drop_table(name) {
        return false;
    }
    for cname in &compiled {
        icat.drop_index(cname);
    }
    true
}

/// Compute a table change with `f` from the catalogs as they stand and
/// install it, again from the new catalogs whenever another change lands
/// first. `Ok(false)` = `f` found nothing to change; `Err` = refused,
/// nothing installed.
fn change_tables(
    ctx: &Ctx<'_>,
    mut f: impl FnMut(&mut TableCatalog, &mut Catalog) -> Result<bool, String>,
) -> Result<bool, String> {
    loop {
        let base = ctx.state.catalog_base();
        let (mut tcat, mut icat) = (base.table_owned(), base.index_owned());
        if !f(&mut tcat, &mut icat)? {
            return Ok(false);
        }
        let change = CatalogChange { index: Some(icat), table: Some(tcat), view: None };
        if ctx.state.commit_catalogs(&base, change) {
            return Ok(true);
        }
    }
}

/// `TABLE.ENSURE …` — `TABLE.DECLARE`'s boot form (dogfood F8.2): the
/// same grammar, but an identical existing declaration is `+UNCHANGED`
/// instead of an error, and a *different* one is a named refusal
/// carrying which part differs. Never a silent rebuild — that cost has
/// its own verb ([`cmd_table_replace`]).
pub(crate) fn cmd_table_ensure<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    sampler: &mut Sampler<'_>,
    args: &A,
    out: &mut Vec<u8>,
) {
    let argv: Vec<&[u8]> = (0..args.len()).map(|i| &args[i] as &[u8]).collect();
    let (spec, globals) = match parse_table_declare_partitioned(&argv) {
        Ok(s) => s,
        Err(e) => return encode_error(out, &e.to_wire()),
    };
    let existing = ctx.state.catalogs.table().and_then(|c| c.get(&spec.name).cloned());
    match existing {
        None => cmd_table_declare(ctx, sampler, args, out),
        Some(cur) if cur.sans_auto() == spec => {
            let names = spec.compile().map(|c| c.into_iter().map(|i| i.name().to_vec()).collect());
            let icat = ctx.state.catalogs.index().map(|c| (*c).clone()).unwrap_or_default();
            if names.is_ok_and(|n: Vec<Vec<u8>>| {
                crate::cmd_table_global::same_spread(&icat, &n, &globals)
            }) {
                out.extend_from_slice(b"+UNCHANGED\r\n");
            } else {
                encode_error(
                    out,
                    "ERR table exists with its paths spread differently (GLOBAL); TABLE.REPLACE rebuilds them",
                );
            }
        }
        Some(cur) => encode_error(out, &spec_diff(&cur.sans_auto(), &spec)),
    }
}

/// `TABLE.REPLACE …` — drop and redeclare, rebuilding every compiled
/// index from the rows. Named for its cost: a full backfill, asked for
/// explicitly. The new spec is validated *before* the old table drops,
/// so a bad replacement leaves the old one standing.
pub(crate) fn cmd_table_replace<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    sampler: &mut Sampler<'_>,
    args: &A,
    out: &mut Vec<u8>,
) {
    let argv: Vec<&[u8]> = (0..args.len()).map(|i| &args[i] as &[u8]).collect();
    let (spec, globals) = match parse_table_declare_partitioned(&argv) {
        Ok(s) => s,
        Err(e) => return encode_error(out, &e.to_wire()),
    };
    if let Err(e) = spec.compile() {
        return encode_error(out, &e.to_wire());
    }
    if sampler.tier_blocked() {
        return encode_error(out, crate::cmd_index::TIER_FLOOR_REFUSAL);
    }
    // two installs, as the verb promises: the drop takes the old indexes
    // away, so the declaration builds every one of them from the rows
    let n = ctx.state.nshards();
    let dropped = change_tables(ctx, |t, i| Ok(dropped_from(t, i, &spec.name)));
    let replaced = dropped
        .and_then(|_| change_tables(ctx, |t, i| declared_onto(t, i, &spec, &globals, sampler, n)));
    match replaced {
        Ok(_) => out.extend_from_slice(b"+OK\r\n"),
        Err(e) => encode_error(out, &e),
    }
}

/// `TABLE.DROP <name>` — drops the table AND its compiled indexes.
pub(crate) fn cmd_table_drop<A: ArgvView + ?Sized>(ctx: &Ctx<'_>, args: &A, out: &mut Vec<u8>) {
    if args.len() != 2 {
        return encode_error(out, "ERR usage: TABLE.DROP name");
    }
    let hit = change_tables(ctx, |t, i| Ok(dropped_from(t, i, &args[1])));
    encode_integer(out, i64::from(hit == Ok(true)));
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
    let Ok(compiled) = spec.compile() else {
        // Catalog entries were admitted validated; an Err here means
        // the sidecar was hand-edited — refuse rather than panic.
        return vec![ST_NOINDEX];
    };
    let mut chunk = vec![ST_OK];
    chunk.extend_from_slice(&(compiled.len() as u32).to_le_bytes());
    for ispec in &compiled {
        match crate::cmd_table_verify::index_verify_counts(ctx, store, ispec.name()) {
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
    let n = spec.compile().map(|c| c.len()).unwrap_or_default();
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
                *slot += u64::from_le_bytes(
                    w.try_into().expect("the get(pos..pos + 8) above returned Some"),
                );
                pos += 8;
            }
        }
        for slot in &mut spot {
            let Some(w) = c.get(pos..pos + 8) else { break };
            *slot += u64::from_le_bytes(
                w.try_into().expect("the get(pos..pos + 8) above returned Some"),
            );
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
    for (ispec, s) in spec.compile().unwrap_or_default().iter().zip(sums) {
        encode_array_len(out, 22);
        encode_bulk(out, b"index");
        encode_bulk(out, ispec.name());
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

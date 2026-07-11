//! IDX.* command surface.
//!
//! Catalog mutations (`IDX.CREATE` / `IDX.DROP`) are Local dispatch
//! handlers (the catalog is process-global; any shard serves them and
//! persists the sidecar). Reads (`IDX.QUERY` / `IDX.COUNT` /
//! `IDX.VERIFY` / `IDX.LIST`) ride the generic extension fan-out:
//! [`extension_op`] computes one shard's chunk (a small private binary
//! encoding), [`extension_reduce`] merges the chunks into RESP at the
//! origin.
//!
//! Cursor contract: the wire cursor is the LAST `(value, key)` served
//! — a point in the global `(value, key)` total order, so every shard
//! resumes exclusively past it. `"0"` = start / exhausted (SCAN
//! convention).

use std::path::Path;

use kevy_index::{Catalog, IndexKind, IndexSpec, ValType};
use kevy_resp::{ArgvView, encode_error, encode_integer};
use crate::state::{Ctx, RuntimeState};

const SIDECAR: &str = "index-catalog.meta";

/// Load a persisted catalog at boot; `serve` calls this once before
/// the reactor starts. A state without a sidecar dir (embedded /
/// test) boots empty.
pub(crate) fn boot(state: &RuntimeState) {
    let Some(dir) = state.sidecar_dir() else { return };
    if let Ok(text) = std::fs::read_to_string(dir.join(SIDECAR))
        && let Some(cat) = Catalog::from_sidecar(&text)
        && !cat.is_empty()
    {
        state.install_index_catalog(cat);
    }
}

fn persist_sidecar(dir: Option<&Path>, cat: &Catalog) {
    let Some(dir) = dir else { return };
    let tmp = dir.join("index-catalog.meta.tmp");
    if std::fs::write(&tmp, cat.to_sidecar()).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join(SIDECAR));
    }
}

// ---------- catalog mutations (Local dispatch) ----------

/// `IDX.CREATE <name> ON PREFIX <p> FIELD <f> TYPE <t> KIND <k>
/// [MAXMEM b] [DIM d] [DISTANCE cosine|l2|ip] [M m] [EF ef]`.
const CREATE_USAGE: &str = "ERR usage: IDX.CREATE name ON PREFIX p FIELD f TYPE i64|f64|str|vector KIND range|unique|text|ann [MAXMEM b] [DIM d] [DISTANCE c] [M m] [EF e]";

pub(crate) fn cmd_idx_create<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    args: &A,
    out: &mut Vec<u8>,
) {
    if args.len() < 11
        || args.len() % 2 != 1
        || !args[2].eq_ignore_ascii_case(b"ON")
        || !args[3].eq_ignore_ascii_case(b"PREFIX")
        || !args[5].eq_ignore_ascii_case(b"FIELD")
        || !args[7].eq_ignore_ascii_case(b"TYPE")
        || !args[9].eq_ignore_ascii_case(b"KIND")
    {
        return encode_error(out, CREATE_USAGE);
    }
    let Ok(opts) = parse_create_opts(args, out) else {
        return;
    };
    let Ok((ty, kind)) = parse_type_kind(args, out) else {
        return;
    };
    let Ok(ann) = validate_kind_combo(kind, ty, &opts, out) else {
        return;
    };
    let spec = IndexSpec {
        name: args[1].to_vec(),
        prefix: args[4].to_vec(),
        field: args[6].to_vec(),
        ty,
        kind,
        max_bytes: opts.max_bytes,
        ann,
        group_by: opts.group_by,
    };
    let mut cat = ctx.state.catalogs.index().map(|c| (*c).clone()).unwrap_or_default();
    match cat.create(spec) {
        Ok(()) => {
            persist_sidecar(ctx.state.sidecar_dir(), &cat);
            ctx.state.install_index_catalog(cat);
            out.extend_from_slice(b"+OK\r\n");
        }
        Err(e) => encode_error(out, e),
    }
}

/// TYPE / KIND / PREFIX validation for IDX.CREATE; an error reply is
/// already written on `Err`.
fn parse_type_kind<A: ArgvView + ?Sized>(
    args: &A,
    out: &mut Vec<u8>,
) -> Result<(ValType, IndexKind), ()> {
    let Some(ty) = ValType::parse(&args[8]) else {
        encode_error(out, "ERR TYPE must be i64|f64|str|vector");
        return Err(());
    };
    let Some(kind) = IndexKind::parse(&args[10]) else {
        encode_error(out, "ERR KIND must be range|unique|text|ann");
        return Err(());
    };
    if args[4].is_empty() {
        encode_error(out, "ERR PREFIX must be non-empty");
        return Err(());
    }
    Ok((ty, kind))
}

/// Optional IDX.CREATE tail (`[MAXMEM b] [DIM d] …`) parsed out.
struct CreateOpts {
    max_bytes: u64,
    dim: u32,
    m: u16,
    ef: u16,
    distance: u8,
    group_by: Option<Vec<u8>>,
}

/// Parse the optional key/value pairs from argv idx 11 on.
/// `Err(())` = an error reply was already encoded into `out`.
fn parse_create_opts<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) -> Result<CreateOpts, ()> {
    let mut max_bytes = 0u64;
    let (mut dim, mut m, mut ef) = (0u32, 16u16, 200u16);
    let mut distance = 0u8;
    let mut group_by: Option<Vec<u8>> = None;
    let mut i = 11;
    while i + 1 < args.len() {
        let (opt, val) = (&args[i], &args[i + 1]);
        let parsed: Option<u64> = std::str::from_utf8(val).ok().and_then(|s| s.parse().ok());
        if opt.eq_ignore_ascii_case(b"MAXMEM") {
            let Some(v) = parsed else {
                encode_error(out, "ERR MAXMEM must be an integer byte count");
                return Err(());
            };
            max_bytes = v;
        } else if opt.eq_ignore_ascii_case(b"DIM") {
            match parsed {
                Some(v) if (1..=65_536).contains(&v) => dim = v as u32,
                _ => { encode_error(out, "ERR DIM must be 1-65536"); return Err(()) },
            }
        } else if opt.eq_ignore_ascii_case(b"M") {
            match parsed {
                Some(v) if (4..=64).contains(&v) => m = v as u16,
                _ => { encode_error(out, "ERR M must be 4-64"); return Err(()) },
            }
        } else if opt.eq_ignore_ascii_case(b"EF") {
            match parsed {
                Some(v) if (16..=1024).contains(&v) => ef = v as u16,
                _ => { encode_error(out, "ERR EF must be 16-1024"); return Err(()) },
            }
        } else if opt.eq_ignore_ascii_case(b"GROUPBY") {
            if val.is_empty() {
                { encode_error(out, "ERR GROUPBY requires a field"); return Err(()) };
            }
            group_by = Some(val.to_vec());
        } else if opt.eq_ignore_ascii_case(b"DISTANCE") {
            match kevy_vector::Distance::parse(val) {
                Some(d) => distance = d as u8,
                None => { encode_error(out, "ERR DISTANCE must be cosine|l2|ip"); return Err(()) },
            }
        } else {
            { encode_error(out, "ERR syntax error"); return Err(()) };
        }
        i += 2;
    }
    Ok(CreateOpts { max_bytes, dim, m, ef, distance, group_by })
}

/// KIND × TYPE (× GROUPBY) compatibility checks; builds the `AnnSpec`
/// for `KIND ann`. `Err(())` = an error reply was already encoded.
fn validate_kind_combo(
    kind: IndexKind,
    ty: ValType,
    opts: &CreateOpts,
    out: &mut Vec<u8>,
) -> Result<Option<kevy_index::AnnSpec>, ()> {
    let ann = match (kind, ty) {
        (IndexKind::Ann, ValType::Vector) if opts.dim > 0 => Some(kevy_index::AnnSpec {
            dim: opts.dim,
            distance: opts.distance,
            m: opts.m,
            ef: opts.ef,
        }),
        (IndexKind::Ann, _) => {
            { encode_error(out, "ERR KIND ann requires TYPE vector and DIM"); return Err(()) };
        }
        (_, ValType::Vector) => {
            { encode_error(out, "ERR TYPE vector requires KIND ann"); return Err(()) };
        }
        _ => None,
    };
    match (kind, &opts.group_by, ty) {
        (IndexKind::Agg, None, _) => {
            { encode_error(out, "ERR KIND agg requires GROUPBY <field>"); Err(()) }
        }
        (IndexKind::Agg, Some(_), ValType::Str | ValType::Vector) => {
            { encode_error(out, "ERR KIND agg requires TYPE i64|f64"); Err(()) }
        }
        (k, Some(_), _) if k != IndexKind::Agg => {
            { encode_error(out, "ERR GROUPBY requires KIND agg"); Err(()) }
        }
        _ => Ok(ann),
    }
}

/// `IDX.DROP <name>`.
pub(crate) fn cmd_idx_drop<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    args: &A,
    out: &mut Vec<u8>,
) {
    if args.len() != 2 {
        return encode_error(out, "ERR usage: IDX.DROP name");
    }
    let mut cat = ctx.state.catalogs.index().map(|c| (*c).clone()).unwrap_or_default();
    let hit = cat.drop_index(&args[1]);
    if hit {
        persist_sidecar(ctx.state.sidecar_dir(), &cat);
        ctx.state.install_index_catalog(cat);
    }
    encode_integer(out, i64::from(hit));
}

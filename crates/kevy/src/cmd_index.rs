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

/// `IDX.CREATE <name> ON PREFIX <p> FIELD <f> | FIELDS <f…> [WEIGHTS <w…>]
/// TYPE <t> KIND <k> [WITH POSITIONS] [VALUES f… [TYPES t…]] [MAXMEM b] [DIM d] [DISTANCE cosine|l2|ip] [M m] [EF ef]`.
/// `FIELDS` and `WITH POSITIONS` are text-only; every other kind takes
/// one `FIELD` and no positions.
const CREATE_USAGE: &str = "ERR usage: IDX.CREATE name ON PREFIX p FIELD f | FIELDS f… [WEIGHTS w…] TYPE i64|f64|str|vector KIND range|unique|text|ann [WITH POSITIONS] [VALUES f… [TYPES t…]] [MAXMEM b] [DIM d] [DISTANCE c] [M m] [EF e]";

/// Parse the field clause and return the fields plus the argv index of
/// the `TYPE` keyword that follows it.
///
/// `FIELD f` is one field at a fixed offset — byte-identical to before,
/// because every existing index test depends on it. `FIELDS f…` scans
/// names until `WEIGHTS` or `TYPE`, then optional matching weights.
/// Only text serves several fields; the catalog refuses the rest, so
/// this parses the shape and lets `create` rule on the kind.
fn parse_fields<A: ArgvView + ?Sized>(
    args: &A,
    out: &mut Vec<u8>,
) -> Result<(Vec<kevy_index::FieldSpec>, usize), ()> {
    if args[5].eq_ignore_ascii_case(b"FIELD") {
        return Ok((vec![kevy_index::FieldSpec::new(args[6].to_vec())], 7));
    }
    if !args[5].eq_ignore_ascii_case(b"FIELDS") {
        encode_error(out, CREATE_USAGE);
        return Err(());
    }
    let stop = |a: &[u8]| a.eq_ignore_ascii_case(b"WEIGHTS") || a.eq_ignore_ascii_case(b"TYPE");
    let mut i = 6;
    let mut names: Vec<Vec<u8>> = Vec::new();
    while i < args.len() && !stop(&args[i]) {
        names.push(args[i].to_vec());
        i += 1;
    }
    if names.is_empty() {
        encode_error(out, "ERR FIELDS needs at least one field name");
        return Err(());
    }
    let mut weights = vec![1.0f32; names.len()];
    if i < args.len() && args[i].eq_ignore_ascii_case(b"WEIGHTS") {
        i = parse_weights(args, i + 1, &mut weights, out)?;
    }
    let fields = names
        .into_iter()
        .zip(weights)
        .map(|(name, weight)| kevy_index::FieldSpec { name, weight })
        .collect();
    Ok((fields, i))
}

/// Fill `weights` from argv starting at `start`, stopping at `TYPE`.
/// Returns the `TYPE` index. Count must match `weights.len()` exactly.
fn parse_weights<A: ArgvView + ?Sized>(
    args: &A,
    start: usize,
    weights: &mut [f32],
    out: &mut Vec<u8>,
) -> Result<usize, ()> {
    let mut i = start;
    let mut wi = 0;
    while i < args.len() && !args[i].eq_ignore_ascii_case(b"TYPE") {
        let Some(w) = std::str::from_utf8(&args[i]).ok().and_then(|s| s.parse::<f32>().ok()) else {
            encode_error(out, "ERR WEIGHTS must be numbers");
            return Err(());
        };
        if wi >= weights.len() {
            encode_error(out, "ERR more WEIGHTS than FIELDS");
            return Err(());
        }
        weights[wi] = w;
        wi += 1;
        i += 1;
    }
    if wi != weights.len() {
        encode_error(out, "ERR WEIGHTS count must match FIELDS count");
        return Err(());
    }
    Ok(i)
}

pub(crate) fn cmd_idx_create<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    store: &kevy_store::Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    if args.len() < 11
        || !args[2].eq_ignore_ascii_case(b"ON")
        || !args[3].eq_ignore_ascii_case(b"PREFIX")
    {
        return encode_error(out, CREATE_USAGE);
    }
    let Ok((fields, type_pos)) = parse_fields(args, out) else {
        return;
    };
    // After the field clause: TYPE t KIND k [opts…]. `type_pos` names
    // the TYPE keyword; opts start four past it and come in pairs.
    if args.len() < type_pos + 4
        || !args[type_pos].eq_ignore_ascii_case(b"TYPE")
        || !args[type_pos + 2].eq_ignore_ascii_case(b"KIND")
        || !(args.len() - (type_pos + 4)).is_multiple_of(2)
    {
        return encode_error(out, CREATE_USAGE);
    }
    let Ok(opts) = parse_create_opts(args, type_pos + 4, out) else {
        return;
    };
    let Ok((ty, kind)) = parse_type_kind(args, type_pos, out) else {
        return;
    };
    let Ok(ann) = validate_kind_combo(kind, ty, &opts, out) else {
        return;
    };
    let spec = IndexSpec {
        name: args[1].to_vec(),
        prefix: args[4].to_vec(),
        fields,
        ty,
        kind,
        max_bytes: opts.max_bytes,
        ann,
        group_by: opts.group_by,
        with_positions: opts.with_positions,
        values: opts.values,
    };
    if !tier_floor_refused(store, out) {
        install_new_index(ctx, spec, out);
    }
}

/// Tiering floor refusal (T5, RFC §4 row 16): indexes are the premium
/// fixed layer demotion can never reclaim — when the existing floor
/// already exhausts the tier's demotable headroom, a new index is
/// refused by name (the FailedOverBudget discipline, moved up to
/// declaration time). Answered from this shard's per-tick gauges; a
/// no-tier store never refuses. `true` = refused (error written).
fn tier_floor_refused(store: &kevy_store::Store, out: &mut Vec<u8>) -> bool {
    if store.tier_index_floor_blocked(0) {
        encode_error(out, "ERR index memory floor exceeds the tiering budget");
        return true;
    }
    false
}

/// Clone the catalog, add `spec`, and on success persist + install it.
fn install_new_index(ctx: &Ctx<'_>, spec: IndexSpec, out: &mut Vec<u8>) {
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
    type_pos: usize,
    out: &mut Vec<u8>,
) -> Result<(ValType, IndexKind), ()> {
    // `type_pos` is the TYPE keyword; value at +1, KIND value at +3.
    // Fixed at 7 for `FIELD f`, scanned for `FIELDS …`.
    let Some(ty) = ValType::parse(&args[type_pos + 1]) else {
        encode_error(out, "ERR TYPE must be i64|f64|str|vector");
        return Err(());
    };
    let Some(kind) = IndexKind::parse(&args[type_pos + 3]) else {
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
    with_positions: bool,
    values: Vec<kevy_index::ValueSpec>,
}

/// The option keywords the CREATE tail understands — the boundary the
/// variadic `VALUES` collects up to.
fn is_create_opt(a: &[u8]) -> bool {
    for kw in [
        b"WITH".as_slice(),
        b"MAXMEM",
        b"DIM",
        b"M",
        b"EF",
        b"GROUPBY",
        b"DISTANCE",
        b"VALUES",
        b"TYPES",
    ] {
        if a.eq_ignore_ascii_case(kw) {
            return true;
        }
    }
    false
}

/// `VALUES f…`: stored field names up to the next option keyword.
/// Variadic like `FIELDS`, and typed the same way `FIELDS` is weighted —
/// by an optional matching list (`TYPES`), defaulting to text.
fn parse_values<A: ArgvView + ?Sized>(
    args: &A,
    start: usize,
    o: &mut CreateOpts,
    out: &mut Vec<u8>,
) -> Result<usize, ()> {
    let mut i = start;
    while i < args.len() && !is_create_opt(&args[i]) {
        o.values.push(kevy_index::ValueSpec::new(args[i].to_vec()));
        i += 1;
    }
    if o.values.is_empty() {
        encode_error(out, "ERR VALUES needs at least one field name");
        return Err(());
    }
    Ok(i)
}

/// `TYPES t…`: how each declared value field's bytes compare. Declared
/// rather than guessed per query, because a numeric range compared
/// lexicographically is silently wrong.
fn parse_value_types<A: ArgvView + ?Sized>(
    args: &A,
    start: usize,
    o: &mut CreateOpts,
    out: &mut Vec<u8>,
) -> Result<usize, ()> {
    let mut i = start;
    let mut n = 0;
    while i < args.len() && !is_create_opt(&args[i]) {
        let Some(ty) = kevy_index::ValType::parse(&args[i]) else {
            encode_error(out, "ERR TYPES must be i64|f64|str");
            return Err(());
        };
        let Some(v) = o.values.get_mut(n) else {
            encode_error(out, "ERR more TYPES than VALUES");
            return Err(());
        };
        v.ty = ty;
        n += 1;
        i += 1;
    }
    if n != o.values.len() {
        encode_error(out, "ERR TYPES count must match VALUES count");
        return Err(());
    }
    Ok(i)
}

/// A parsed integer clamped to `[lo, hi]`, or an error already written.
fn ranged(parsed: Option<u64>, lo: u64, hi: u64, msg: &str, out: &mut Vec<u8>) -> Result<u64, ()> {
    match parsed {
        Some(v) if (lo..=hi).contains(&v) => Ok(v),
        _ => {
            encode_error(out, msg);
            Err(())
        }
    }
}

/// Parse the optional key/value pairs after `TYPE t KIND k`.
/// `Err(())` = an error reply was already encoded into `out`.
fn parse_create_opts<A: ArgvView + ?Sized>(
    args: &A,
    start: usize,
    out: &mut Vec<u8>,
) -> Result<CreateOpts, ()> {
    let mut o = CreateOpts {
        max_bytes: 0,
        dim: 0,
        m: 16,
        ef: 200,
        distance: 0,
        group_by: None,
        with_positions: false,
        values: Vec::new(),
    };
    let mut i = start;
    while i < args.len() {
        // `VALUES f…` is variadic, like `FIELDS`: it collects names up to
        // the next option keyword. Everything else is a key/value pair.
        if args[i].eq_ignore_ascii_case(b"VALUES") {
            i = parse_values(args, i + 1, &mut o, out)?;
            continue;
        }
        if args[i].eq_ignore_ascii_case(b"TYPES") {
            i = parse_value_types(args, i + 1, &mut o, out)?;
            continue;
        }
        if i + 1 >= args.len() {
            break;
        }
        apply_create_opt(&args[i], &args[i + 1], &mut o, out)?;
        i += 2;
    }
    Ok(o)
}

/// Apply one `KEY value` option pair to the accumulating [`CreateOpts`].
/// `Err(())` = an error reply was already encoded.
fn apply_create_opt(opt: &[u8], val: &[u8], o: &mut CreateOpts, out: &mut Vec<u8>) -> Result<(), ()> {
    let parsed: Option<u64> = std::str::from_utf8(val).ok().and_then(|s| s.parse().ok());
    if opt.eq_ignore_ascii_case(b"WITH") {
        // A bare flag written as a key/value pair so it fits the
        // even-arity tail: `WITH POSITIONS`. Text-only; `create` rules
        // on the kind.
        if !val.eq_ignore_ascii_case(b"POSITIONS") {
            encode_error(out, "ERR WITH only accepts POSITIONS");
            return Err(());
        }
        o.with_positions = true;
    } else if opt.eq_ignore_ascii_case(b"MAXMEM") {
        let Some(v) = parsed else {
            encode_error(out, "ERR MAXMEM must be an integer byte count");
            return Err(());
        };
        o.max_bytes = v;
    } else if opt.eq_ignore_ascii_case(b"DIM") {
        o.dim = ranged(parsed, 1, 65_536, "ERR DIM must be 1-65536", out)? as u32;
    } else if opt.eq_ignore_ascii_case(b"M") {
        o.m = ranged(parsed, 4, 64, "ERR M must be 4-64", out)? as u16;
    } else if opt.eq_ignore_ascii_case(b"EF") {
        o.ef = ranged(parsed, 16, 1024, "ERR EF must be 16-1024", out)? as u16;
    } else if opt.eq_ignore_ascii_case(b"GROUPBY") {
        if val.is_empty() {
            encode_error(out, "ERR GROUPBY requires a field");
            return Err(());
        }
        o.group_by = Some(val.to_vec());
    } else if opt.eq_ignore_ascii_case(b"DISTANCE") {
        match kevy_vector::Distance::parse(val) {
            Some(d) => o.distance = d as u8,
            None => {
                encode_error(out, "ERR DISTANCE must be cosine|l2|ip");
                return Err(());
            }
        }
    } else {
        encode_error(out, "ERR syntax error");
        return Err(());
    }
    Ok(())
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

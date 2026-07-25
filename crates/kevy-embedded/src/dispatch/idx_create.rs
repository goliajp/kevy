//! `IDX.CREATE` parser for the embedded RESP dispatch.
//!
//! Grammar, error strings, and validation ORDER mirror the server's
//! `crates/kevy/src/cmd_index.rs` byte-for-byte (the `dispatch_oracle`
//! test compares the two wire replies), then the parsed spec routes to
//! the Store's typed `idx_create*` capability. Split out of `idx.rs`
//! for the 500-LOC house rule.
//!
//! Full wire syntax:
//! `IDX.CREATE name ON PREFIX p FIELD f | FIELDS f… [WEIGHTS w…]
//!  TYPE t KIND k [WITH POSITIONS] [VALUES f… [TYPES t…]]
//!  [MAXMEM b] [DIM d] [DISTANCE c] [M m] [EF e]`.

use kevy_index::{FieldSpec, IndexKind, ValType, ValueSpec};

use super::util::{err, kevy_err};
use crate::store::Store;

pub(super) const CREATE_USAGE: &str = "ERR usage: IDX.CREATE name ON PREFIX p FIELD f | FIELDS f… [WEIGHTS w…] TYPE i64|f64|str|vector KIND range|unique|text|ann [WITH POSITIONS] [VALUES f… [TYPES t…]] [MAXMEM b] [DIM d] [DISTANCE c] [M m] [EF e]";

/// Optional CREATE tail, parsed out. MAXMEM is accepted then ignored
/// (the embedded build is synchronous, with no budget). Without the
/// `vector` feature the ANN knobs are parsed for grammar parity but
/// never read (KIND ann errors before reaching them).
#[cfg_attr(not(feature = "vector"), allow(dead_code))]
struct CreateOpts {
    dim: u32,
    m: u16,
    ef: u16,
    distance: u8,
    group_by: Option<Vec<u8>>,
    with_positions: bool,
    values: Vec<ValueSpec>,
}

/// The fully parsed shape a valid IDX.CREATE resolves to.
struct Parsed {
    fields: Vec<FieldSpec>,
    ty: ValType,
    kind: IndexKind,
    opts: CreateOpts,
}

/// `IDX.CREATE …` — parse (mirroring the server) then route to the
/// Store. A parse error is written verbatim; the build's own result
/// (`+OK` / catalog error) rides `kevy_err`.
pub(super) fn cmd_idx_create(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    match parse(argv) {
        Ok(p) => route(s, argv, &p, out),
        Err(msg) => err(out, msg),
    }
}

/// Full parse in the server's order: header → fields → TYPE/KIND arity
/// → opts → TYPE/KIND values → kind combo. `Err` = the error string.
fn parse(argv: &[Vec<u8>]) -> Result<Parsed, &'static str> {
    if argv.len() < 11
        || !argv[2].eq_ignore_ascii_case(b"ON")
        || !argv[3].eq_ignore_ascii_case(b"PREFIX")
    {
        return Err(CREATE_USAGE);
    }
    let (fields, type_pos) = parse_fields(argv)?;
    // After the field clause: TYPE t KIND k [opts…]. `type_pos` names
    // the TYPE keyword; opts start four past it and come in pairs.
    if argv.len() < type_pos + 4
        || !argv[type_pos].eq_ignore_ascii_case(b"TYPE")
        || !argv[type_pos + 2].eq_ignore_ascii_case(b"KIND")
        || !(argv.len() - (type_pos + 4)).is_multiple_of(2)
    {
        return Err(CREATE_USAGE);
    }
    let opts = parse_create_opts(argv, type_pos + 4)?;
    let (ty, kind) = parse_type_kind(argv, type_pos)?;
    validate_kind_combo(kind, ty, &opts)?;
    Ok(Parsed { fields, ty, kind, opts })
}

/// Parse the field clause; return the fields plus the argv index of the
/// `TYPE` keyword. `FIELD f` is one field at a fixed offset (7, so every
/// single-field test stays byte-identical); `FIELDS f…` scans names to
/// `WEIGHTS`/`TYPE`, then optional matching weights.
fn parse_fields(argv: &[Vec<u8>]) -> Result<(Vec<FieldSpec>, usize), &'static str> {
    if argv[5].eq_ignore_ascii_case(b"FIELD") {
        return Ok((vec![FieldSpec::new(argv[6].to_vec())], 7));
    }
    if !argv[5].eq_ignore_ascii_case(b"FIELDS") {
        return Err(CREATE_USAGE);
    }
    let stop = |a: &[u8]| a.eq_ignore_ascii_case(b"WEIGHTS") || a.eq_ignore_ascii_case(b"TYPE");
    let mut i = 6;
    let mut names: Vec<Vec<u8>> = Vec::new();
    while i < argv.len() && !stop(&argv[i]) {
        names.push(argv[i].to_vec());
        i += 1;
    }
    if names.is_empty() {
        return Err("ERR FIELDS needs at least one field name");
    }
    let mut weights = vec![1.0f32; names.len()];
    if i < argv.len() && argv[i].eq_ignore_ascii_case(b"WEIGHTS") {
        i = parse_weights(argv, i + 1, &mut weights)?;
    }
    let fields = names
        .into_iter()
        .zip(weights)
        .map(|(name, weight)| FieldSpec { name, weight })
        .collect();
    Ok((fields, i))
}

/// Fill `weights` from argv starting at `start`, stopping at `TYPE`.
/// Returns the `TYPE` index. Count must match `weights.len()` exactly.
fn parse_weights(argv: &[Vec<u8>], start: usize, weights: &mut [f32]) -> Result<usize, &'static str> {
    let mut i = start;
    let mut wi = 0;
    while i < argv.len() && !argv[i].eq_ignore_ascii_case(b"TYPE") {
        let Some(w) = std::str::from_utf8(&argv[i]).ok().and_then(|s| s.parse::<f32>().ok()) else {
            return Err("ERR WEIGHTS must be numbers");
        };
        if wi >= weights.len() {
            return Err("ERR more WEIGHTS than FIELDS");
        }
        weights[wi] = w;
        wi += 1;
        i += 1;
    }
    if wi != weights.len() {
        return Err("ERR WEIGHTS count must match FIELDS count");
    }
    Ok(i)
}

/// TYPE / KIND / PREFIX validation. `type_pos` is the TYPE keyword;
/// value at +1, KIND value at +3.
fn parse_type_kind(argv: &[Vec<u8>], type_pos: usize) -> Result<(ValType, IndexKind), &'static str> {
    let Some(ty) = ValType::parse(&argv[type_pos + 1]) else {
        return Err("ERR TYPE must be i64|f64|str|vector");
    };
    let Some(kind) = IndexKind::parse(&argv[type_pos + 3]) else {
        return Err("ERR KIND must be range|unique|text|ann");
    };
    if argv[4].is_empty() {
        return Err("ERR PREFIX must be non-empty");
    }
    Ok((ty, kind))
}

/// The option keywords the CREATE tail understands — the boundary the
/// variadic `VALUES` / `TYPES` lists collect up to.
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

/// Parse the optional key/value pairs after `TYPE t KIND k`, plus the
/// variadic `VALUES` / `TYPES` lists. `Err` = the error string.
fn parse_create_opts(argv: &[Vec<u8>], start: usize) -> Result<CreateOpts, &'static str> {
    let mut o = CreateOpts {
        dim: 0,
        m: 16,
        ef: 200,
        distance: 0,
        group_by: None,
        with_positions: false,
        values: Vec::new(),
    };
    let mut i = start;
    while i < argv.len() {
        // `VALUES f…` is variadic, like `FIELDS`: it collects names up to
        // the next option keyword. Everything else is a key/value pair.
        if argv[i].eq_ignore_ascii_case(b"VALUES") {
            i = parse_values(argv, i + 1, &mut o)?;
            continue;
        }
        if argv[i].eq_ignore_ascii_case(b"TYPES") {
            i = parse_value_types(argv, i + 1, &mut o)?;
            continue;
        }
        if i + 1 >= argv.len() {
            break;
        }
        apply_create_opt(&argv[i], &argv[i + 1], &mut o)?;
        i += 2;
    }
    Ok(o)
}

/// `VALUES f…`: stored field names up to the next option keyword.
fn parse_values(argv: &[Vec<u8>], start: usize, o: &mut CreateOpts) -> Result<usize, &'static str> {
    let mut i = start;
    while i < argv.len() && !is_create_opt(&argv[i]) {
        o.values.push(ValueSpec::new(argv[i].to_vec()));
        i += 1;
    }
    if o.values.is_empty() {
        return Err("ERR VALUES needs at least one field name");
    }
    Ok(i)
}

/// `TYPES t…`: how each declared value field's bytes compare. Declared
/// rather than guessed, because a numeric range compared
/// lexicographically is silently wrong.
fn parse_value_types(argv: &[Vec<u8>], start: usize, o: &mut CreateOpts) -> Result<usize, &'static str> {
    let mut i = start;
    let mut n = 0;
    while i < argv.len() && !is_create_opt(&argv[i]) {
        let Some(ty) = ValType::parse(&argv[i]) else {
            return Err("ERR TYPES must be i64|f64|str");
        };
        let Some(v) = o.values.get_mut(n) else {
            return Err("ERR more TYPES than VALUES");
        };
        v.ty = ty;
        n += 1;
        i += 1;
    }
    if n != o.values.len() {
        return Err("ERR TYPES count must match VALUES count");
    }
    Ok(i)
}

/// A parsed integer clamped to `[lo, hi]`, or the error string.
fn ranged(parsed: Option<u64>, lo: u64, hi: u64, msg: &'static str) -> Result<u64, &'static str> {
    match parsed {
        Some(v) if (lo..=hi).contains(&v) => Ok(v),
        _ => Err(msg),
    }
}

/// Apply one `KEY value` option pair to the accumulating [`CreateOpts`].
fn apply_create_opt(opt: &[u8], val: &[u8], o: &mut CreateOpts) -> Result<(), &'static str> {
    let parsed: Option<u64> = std::str::from_utf8(val).ok().and_then(|s| s.parse().ok());
    if opt.eq_ignore_ascii_case(b"WITH") {
        // A bare flag as a key/value pair so it fits the even-arity tail:
        // `WITH POSITIONS`. Text-only; the catalog rules on the kind.
        if !val.eq_ignore_ascii_case(b"POSITIONS") {
            return Err("ERR WITH only accepts POSITIONS");
        }
        o.with_positions = true;
    } else if opt.eq_ignore_ascii_case(b"MAXMEM") {
        // Validated, then ignored: no build budget embedded.
        parsed.ok_or("ERR MAXMEM must be an integer byte count")?;
    } else if opt.eq_ignore_ascii_case(b"DIM") {
        o.dim = ranged(parsed, 1, 65_536, "ERR DIM must be 1-65536")? as u32;
    } else if opt.eq_ignore_ascii_case(b"M") {
        o.m = ranged(parsed, 4, 64, "ERR M must be 4-64")? as u16;
    } else if opt.eq_ignore_ascii_case(b"EF") {
        o.ef = ranged(parsed, 16, 1024, "ERR EF must be 16-1024")? as u16;
    } else if opt.eq_ignore_ascii_case(b"GROUPBY") {
        if val.is_empty() {
            return Err("ERR GROUPBY requires a field");
        }
        o.group_by = Some(val.to_vec());
    } else if opt.eq_ignore_ascii_case(b"DISTANCE") {
        o.distance = if val.eq_ignore_ascii_case(b"cosine") {
            0
        } else if val.eq_ignore_ascii_case(b"l2") {
            1
        } else if val.eq_ignore_ascii_case(b"ip") {
            2
        } else {
            return Err("ERR DISTANCE must be cosine|l2|ip");
        };
    } else {
        return Err("ERR syntax error");
    }
    Ok(())
}

/// KIND × TYPE (× GROUPBY) compatibility, mirroring the server.
fn validate_kind_combo(kind: IndexKind, ty: ValType, opts: &CreateOpts) -> Result<(), &'static str> {
    match (kind, ty) {
        (IndexKind::Ann, ValType::Vector) if opts.dim > 0 => {}
        (IndexKind::Ann, _) => return Err("ERR KIND ann requires TYPE vector and DIM"),
        (_, ValType::Vector) => return Err("ERR TYPE vector requires KIND ann"),
        _ => {}
    }
    match (kind, &opts.group_by, ty) {
        (IndexKind::Agg, None, _) => Err("ERR KIND agg requires GROUPBY <field>"),
        (IndexKind::Agg, Some(_), ValType::Str | ValType::Vector) => {
            Err("ERR KIND agg requires TYPE i64|f64")
        }
        (k, Some(_), _) if k != IndexKind::Agg => Err("ERR GROUPBY requires KIND agg"),
        _ => Ok(()),
    }
}

/// Route a parsed spec to the Store's typed capability. Text carries
/// the full multi-field / positions / values shape; every other kind
/// takes the first field. The feature gates mirror `Store`: text needs
/// `text`, ann needs `vector`.
fn route(s: &Store, argv: &[Vec<u8>], p: &Parsed, out: &mut Vec<u8>) {
    let (name, prefix) = (&argv[1], &argv[4]);
    let field0 = &p.fields[0].name;
    // VALUES rides text and the scalar kinds; on ann / agg the catalog
    // refuses it — the server's exact wording, produced here because
    // the typed `idx_create_agg` / `idx_create_ann` capabilities do not
    // carry a values list to be refused downstream.
    if !p.opts.values.is_empty() && matches!(p.kind, IndexKind::Agg | IndexKind::Ann) {
        return err(out, "ERR VALUES requires KIND text|range|unique");
    }
    let res = match p.kind {
        #[cfg(feature = "text")]
        IndexKind::Text => create_text(s, name, prefix, p),
        IndexKind::Agg => {
            s.idx_create_agg(name, prefix, field0, p.ty, p.opts.group_by.as_deref().unwrap_or(b""))
        }
        #[cfg(feature = "vector")]
        IndexKind::Ann => s.idx_create_ann(
            name,
            prefix,
            field0,
            kevy_index::AnnSpec { dim: p.opts.dim, distance: p.opts.distance, m: p.opts.m, ef: p.opts.ef },
        ),
        #[cfg(not(feature = "vector"))]
        IndexKind::Ann => return err(out, "ERR vector indexes need the `vector` feature"),
        // range / unique (and text / ann when their feature is off — the
        // Store returns the same feature error the server would).
        _ if p.opts.values.is_empty() => s.idx_create(name, prefix, field0, p.ty, p.kind),
        _ => {
            let values: Vec<(&[u8], ValType)> =
                p.opts.values.iter().map(|v| (v.name.as_slice(), v.ty)).collect();
            s.idx_create_with_values(name, prefix, field0, p.ty, p.kind, &values)
        }
    };
    match res {
        Ok(()) => out.extend_from_slice(b"+OK\r\n"),
        Err(e) => kevy_err(out, &e),
    }
}

/// Declare a text index with its full shape: weighted fields, optional
/// positions, and typed stored values.
#[cfg(feature = "text")]
fn create_text(s: &Store, name: &[u8], prefix: &[u8], p: &Parsed) -> crate::KevyResult<()> {
    let fields: Vec<(&[u8], f32)> = p.fields.iter().map(|f| (f.name.as_slice(), f.weight)).collect();
    let values: Vec<(&[u8], ValType)> =
        p.opts.values.iter().map(|v| (v.name.as_slice(), v.ty)).collect();
    s.idx_create_text(name, prefix, &fields, p.opts.with_positions, &values)
}

//! v2.5 — IDX.* command surface (RFC LOCKED).
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

use std::path::PathBuf;
use std::sync::OnceLock;

use kevy_index::{Catalog, IndexKind, IndexSpec, ValType};
use kevy_resp::{ArgvView, encode_error, encode_integer};
use crate::index_runtime;

/// Data dir for the catalog sidecar (set once by `serve`).
static SIDECAR_DIR: OnceLock<PathBuf> = OnceLock::new();
const SIDECAR: &str = "index-catalog.meta";

/// Install the sidecar dir + load a persisted catalog at boot.
pub(crate) fn boot(data_dir: &std::path::Path) {
    let _ = SIDECAR_DIR.set(data_dir.to_path_buf());
    if let Ok(text) = std::fs::read_to_string(data_dir.join(SIDECAR))
        && let Some(cat) = Catalog::from_sidecar(&text)
        && !cat.is_empty()
    {
        index_runtime::install_catalog(cat);
    }
}

/// v2.6: the view catalog persists next to the index catalog.
pub(crate) fn sidecar_dir() -> Option<&'static std::path::Path> {
    SIDECAR_DIR.get().map(PathBuf::as_path)
}

fn persist_sidecar(cat: &Catalog) {
    if let Some(dir) = SIDECAR_DIR.get() {
        let tmp = dir.join("index-catalog.meta.tmp");
        if std::fs::write(&tmp, cat.to_sidecar()).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join(SIDECAR));
        }
    }
}

// ---------- catalog mutations (Local dispatch) ----------

/// `IDX.CREATE <name> ON PREFIX <p> FIELD <f> TYPE <t> KIND <k>
/// [MAXMEM b] [DIM d] [DISTANCE cosine|l2|ip] [M m] [EF ef]`.
pub(crate) fn cmd_idx_create<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
    if args.len() < 11
        || args.len() % 2 != 1
        || !args[2].eq_ignore_ascii_case(b"ON")
        || !args[3].eq_ignore_ascii_case(b"PREFIX")
        || !args[5].eq_ignore_ascii_case(b"FIELD")
        || !args[7].eq_ignore_ascii_case(b"TYPE")
        || !args[9].eq_ignore_ascii_case(b"KIND")
    {
        return encode_error(
            out,
            "ERR usage: IDX.CREATE name ON PREFIX p FIELD f TYPE i64|f64|str|vector KIND range|unique|text|ann [MAXMEM b] [DIM d] [DISTANCE c] [M m] [EF e]",
        );
    }
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
                return encode_error(out, "ERR MAXMEM must be an integer byte count");
            };
            max_bytes = v;
        } else if opt.eq_ignore_ascii_case(b"DIM") {
            match parsed {
                Some(v) if (1..=65_536).contains(&v) => dim = v as u32,
                _ => return encode_error(out, "ERR DIM must be 1-65536"),
            }
        } else if opt.eq_ignore_ascii_case(b"M") {
            match parsed {
                Some(v) if (4..=64).contains(&v) => m = v as u16,
                _ => return encode_error(out, "ERR M must be 4-64"),
            }
        } else if opt.eq_ignore_ascii_case(b"EF") {
            match parsed {
                Some(v) if (16..=1024).contains(&v) => ef = v as u16,
                _ => return encode_error(out, "ERR EF must be 16-1024"),
            }
        } else if opt.eq_ignore_ascii_case(b"GROUPBY") {
            if val.is_empty() {
                return encode_error(out, "ERR GROUPBY requires a field");
            }
            group_by = Some(val.to_vec());
        } else if opt.eq_ignore_ascii_case(b"DISTANCE") {
            match kevy_vector::Distance::parse(val) {
                Some(d) => distance = d as u8,
                None => return encode_error(out, "ERR DISTANCE must be cosine|l2|ip"),
            }
        } else {
            return encode_error(out, "ERR syntax error");
        }
        i += 2;
    }
    let Some(ty) = ValType::parse(&args[8]) else {
        return encode_error(out, "ERR TYPE must be i64|f64|str|vector");
    };
    let Some(kind) = IndexKind::parse(&args[10]) else {
        return encode_error(out, "ERR KIND must be range|unique|text|ann");
    };
    if args[4].is_empty() {
        return encode_error(out, "ERR PREFIX must be non-empty");
    }
    let ann = match (kind, ty) {
        (IndexKind::Ann, ValType::Vector) if dim > 0 => {
            Some(kevy_index::AnnSpec { dim, distance, m, ef })
        }
        (IndexKind::Ann, _) => {
            return encode_error(out, "ERR KIND ann requires TYPE vector and DIM");
        }
        (_, ValType::Vector) => {
            return encode_error(out, "ERR TYPE vector requires KIND ann");
        }
        _ => None,
    };
    match (kind, &group_by, ty) {
        (IndexKind::Agg, None, _) => {
            return encode_error(out, "ERR KIND agg requires GROUPBY <field>");
        }
        (IndexKind::Agg, Some(_), ValType::Str | ValType::Vector) => {
            return encode_error(out, "ERR KIND agg requires TYPE i64|f64");
        }
        (k, Some(_), _) if k != IndexKind::Agg => {
            return encode_error(out, "ERR GROUPBY requires KIND agg");
        }
        _ => {}
    }
    let spec = IndexSpec {
        name: args[1].to_vec(),
        prefix: args[4].to_vec(),
        field: args[6].to_vec(),
        ty,
        kind,
        max_bytes,
        ann,
        group_by,
        };
    let mut cat = index_runtime::catalog().map(|c| (*c).clone()).unwrap_or_default();
    match cat.create(spec) {
        Ok(()) => {
            persist_sidecar(&cat);
            index_runtime::install_catalog(cat);
            out.extend_from_slice(b"+OK\r\n");
        }
        Err(e) => encode_error(out, e),
    }
}

/// `IDX.DROP <name>`.
pub(crate) fn cmd_idx_drop<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
    if args.len() != 2 {
        return encode_error(out, "ERR usage: IDX.DROP name");
    }
    let mut cat = index_runtime::catalog().map(|c| (*c).clone()).unwrap_or_default();
    let hit = cat.drop_index(&args[1]);
    if hit {
        persist_sidecar(&cat);
        index_runtime::install_catalog(cat);
    }
    encode_integer(out, i64::from(hit));
}

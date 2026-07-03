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

/// `IDX.CREATE <name> ON PREFIX <p> FIELD <f> TYPE <t> KIND <k>`.
pub(crate) fn cmd_idx_create<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
    let with_maxmem = args.len() == 13;
    if !(args.len() == 11 || with_maxmem)
        || !args[2].eq_ignore_ascii_case(b"ON")
        || !args[3].eq_ignore_ascii_case(b"PREFIX")
        || !args[5].eq_ignore_ascii_case(b"FIELD")
        || !args[7].eq_ignore_ascii_case(b"TYPE")
        || !args[9].eq_ignore_ascii_case(b"KIND")
        || (with_maxmem && !args[11].eq_ignore_ascii_case(b"MAXMEM"))
    {
        return encode_error(
            out,
            "ERR usage: IDX.CREATE name ON PREFIX p FIELD f TYPE i64|f64|str KIND range|unique [MAXMEM bytes]",
        );
    }
    let max_bytes: u64 = if with_maxmem {
        match std::str::from_utf8(&args[12]).ok().and_then(|s| s.parse().ok()) {
            Some(v) => v,
            None => return encode_error(out, "ERR MAXMEM must be an integer byte count"),
        }
    } else {
        0
    };
    let Some(ty) = ValType::parse(&args[8]) else {
        return encode_error(out, "ERR TYPE must be i64|f64|str");
    };
    let Some(kind) = IndexKind::parse(&args[10]) else {
        return encode_error(out, "ERR KIND must be range|unique");
    };
    if args[4].is_empty() {
        return encode_error(out, "ERR PREFIX must be non-empty");
    }
    let spec = IndexSpec {
        name: args[1].to_vec(),
        prefix: args[4].to_vec(),
        field: args[6].to_vec(),
        ty,
        kind,
        max_bytes,
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

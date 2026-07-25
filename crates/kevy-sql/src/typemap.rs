//! The SQL → kevy type mapping — deliberately coarse and documented as
//! such: kevy columns are `i64 | f64 | str`, nothing else. Timestamps,
//! UUIDs, JSON and booleans all land on `str` (app-encoded); the notes
//! the compiler emits say so per column, honestly.

use crate::KevyType;

/// Map one (lower-cased) SQL type name. `None` = not in the subset.
/// `double precision` is handled by the parser (two words) and arrives
/// here as `"double precision"`.
pub(crate) fn map_type(name: &str) -> Option<KevyType> {
    Some(match name {
        "int" | "integer" | "bigint" | "serial" | "bigserial" => KevyType::I64,
        "real" | "float" | "double precision" | "numeric" | "decimal" => KevyType::F64,
        "text" | "varchar" | "char" | "uuid" | "timestamp" | "timestamptz" | "date" | "bool"
        | "boolean" | "json" | "jsonb" => KevyType::Str,
        _ => return None,
    })
}

/// Whether the type name accepts `(n[, m])` precision arguments.
pub(crate) fn takes_args(name: &str) -> bool {
    matches!(name, "varchar" | "char" | "numeric" | "decimal" | "float")
}

/// The honest-mapping note for one column, when the mapping loses
/// something worth saying. `None` = the mapping is lossless enough
/// (int/bigint/text/varchar/…).
pub(crate) fn mapping_note(table: &str, column: &str, sql_ty: &str) -> Option<String> {
    let tail = match sql_ty {
        "serial" | "bigserial" => {
            "i64, but ids do NOT auto-increment — allocate them app-side (INCR block, cookbook \u{a7}3)"
        }
        "numeric" | "decimal" => {
            "f64 — fixed-point precision becomes binary float; keep money as integer cents if exactness matters"
        }
        "timestamp" | "timestamptz" | "date" => {
            "str — kevy stores app-encoded time; use a fixed-width sortable encoding (RFC3339 or zero-padded epoch) so ranges order correctly"
        }
        "json" | "jsonb" => {
            "str — flatten the paths you index into their own columns (cookbook \u{a7}9); JSON-path queries are permanently out"
        }
        "bool" | "boolean" => "str — store '0'/'1'",
        "uuid" => "str — stored as its text form",
        _ => return None,
    };
    Some(format!("{table}.{column}: {sql_ty} \u{2192} {tail}"))
}

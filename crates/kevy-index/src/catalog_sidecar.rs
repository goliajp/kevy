//! The catalog sidecar text format, split from `catalog.rs` for the
//! 500-LOC house rule.
//!
//! One line per index. v1 wrote a single bare field name in column 3; v2
//! writes `name:weight`, comma-separated, for multi-attribute indexes.
//! **v1 stays readable forever** — it is on disk in every store written
//! before multi-field, and a sidecar that refuses to load is not an
//! error an operator sees; it is every index silently rebuilding from
//! scratch on the next boot.

use core::fmt::Write as _;

use crate::catalog::{AnnSpec, Catalog, FieldSpec, IndexKind, IndexSpec, ValType, ValueSpec};

impl Catalog {
    /// Serialize to the sidecar text form (one line per index:
    /// `name<TAB>prefix<TAB>field<TAB>ty<TAB>kind<TAB>max_bytes[<TAB>ann]`,
    /// fields hex-escaped for tabs/newlines via `%XX`; the 7th column
    /// is `dim,distance,m,ef` for ANN kinds).
    pub fn to_sidecar(&self) -> String {
        // Write the OLDEST header that can represent the data: v5 exists
        // only for scalar-kind VALUES (capacity arc G1), so a catalog
        // without one serializes byte-identically to the v4 writer (A5)
        // — and stays readable by every pre-G1 binary.
        let needs_v5 = self.specs.iter().any(|(s, _)| {
            matches!(s.kind, IndexKind::Range | IndexKind::Unique) && !s.values.is_empty()
        });
        let mut out =
            String::from(if needs_v5 { "kevy-index-catalog v5\n" } else { "kevy-index-catalog v4\n" });
        for (s, _) in &self.specs {
            let _ = write!(
                out,
                "{}\t{}\t{}\t{}\t{}\t{}",
                esc(&s.name),
                esc(&s.prefix),
                fields_to_col(&s.fields),
                s.ty.tag(),
                s.kind.tag(),
                s.max_bytes
            );
            // 7th column is kind-interpreted: ann params for Ann,
            // escaped group field for Agg, and `pos` for a text index
            // created WITH POSITIONS (v3). The three are mutually
            // exclusive — a kind is at most one of Ann / Agg / Text.
            if let Some(a) = &s.ann {
                let _ = write!(out, "\t{},{},{},{}", a.dim, a.distance, a.m, a.ef);
            } else if let Some(g) = &s.group_by {
                let _ = write!(out, "\t{}", esc(g));
            } else if let Some(col) = values_col(s) {
                // Text (v3+) and, from v5, the scalar kinds: the same
                // `pos|-,name:ty,…` column, `pos` being text-only.
                let _ = write!(out, "\t{col}");
            }
            out.push('\n');
        }
        out
    }

    /// Parse the sidecar text form; all indexes load as `Building`
    /// (boot rebuild). `None` on malformed input.
    pub fn from_sidecar(text: &str) -> Option<Catalog> {
        let mut lines = text.lines();
        // v1 stays readable forever: it is on disk in every store written
        // before multi-field, and a sidecar that refuses to load is an
        // index that silently rebuilds from scratch. Same contract as the
        // AOF envelope -- read both, write the current one, upgrade on
        // the next rewrite.
        // Version is a number, not a bool, now that v3 adds a text
        // positions flag on top of v2's weighted fields. v1/v2 stay
        // readable forever; only the writer moves to the newest form.
        let version: u8 = match lines.next()? {
            "kevy-index-catalog v5" => 5,
            "kevy-index-catalog v4" => 4,
            "kevy-index-catalog v3" => 3,
            "kevy-index-catalog v2" => 2,
            "kevy-index-catalog v1" => 1,
            _ => return None,
        };
        let mut c = Catalog::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            c.create(spec_from_line(line, version)?).ok()?;
        }
        Some(c)
    }}

fn spec_from_line(line: &str, version: u8) -> Option<IndexSpec> {
    let parts: Vec<&str> = line.split('\t').collect();
    if !(parts.len() == 6 || parts.len() == 7) {
        return None;
    }
    let kind = IndexKind::parse(parts[4].as_bytes())?;
    let (ann, group_by, with_positions) = if parts.len() == 7 {
        match kind {
            IndexKind::Ann => (Some(ann_col(parts[6])?), None, false),
            IndexKind::Agg => (None, Some(unesc(parts[6])?), false),
            // A text index's 7th column carries its own flags; older
            // sidecars never wrote one, so it only appears from v3 on.
            IndexKind::Text if version >= 3 => return text_spec(&parts, version),
            // A scalar kind's 7th column is its VALUES list (v5 — the
            // capacity arc's G1). `pos` stays text-only: a positions
            // head on a scalar line is malformed, not ignored.
            IndexKind::Range | IndexKind::Unique if version >= 5 => {
                return scalar_spec(&parts, version);
            }
            _ => return None,
        }
    } else {
        (None, None, false)
    };
    Some(IndexSpec {
        name: unesc(parts[0])?,
        prefix: unesc(parts[1])?,
        fields: col_to_fields(parts[2], version >= 2)?,
        ty: ValType::parse(parts[3].as_bytes())?,
        kind,
        max_bytes: parts[5].parse().ok()?,
        ann,
        group_by,
        with_positions,
        values: Vec::new(),
    })
}

/// An ANN line's 7th column: `dim,distance,m,ef`.
fn ann_col(col: &str) -> Option<AnnSpec> {
    let nums: Vec<&str> = col.split(',').collect();
    if nums.len() != 4 {
        return None;
    }
    Some(AnnSpec {
        dim: nums[0].parse().ok()?,
        distance: nums[1].parse().ok()?,
        m: nums[2].parse().ok()?,
        ef: nums[3].parse().ok()?,
    })
}

/// A text index's line, whose 7th column is its own flags rather than
/// the ann / agg forms the shared path handles.
fn text_spec(parts: &[&str], version: u8) -> Option<IndexSpec> {
    let (with_positions, values) = parse_text_col(parts[6])?;
    Some(IndexSpec {
        name: unesc(parts[0])?,
        prefix: unesc(parts[1])?,
        fields: col_to_fields(parts[2], version >= 2)?,
        ty: ValType::parse(parts[3].as_bytes())?,
        kind: IndexKind::Text,
        max_bytes: parts[5].parse().ok()?,
        ann: None,
        group_by: None,
        with_positions,
        values,
    })
}

/// The `VALUES`-bearing 7th column (text v3+, scalar kinds v5+), or
/// `None` when the spec has nothing to say.
///
/// Comma-separated: the head is `pos` or `-` for the positions flag, the
/// tail is the declared `VALUES` field names. A positions-only index
/// therefore writes exactly `pos` — the v3 form — so the shape only
/// changes for an index that actually uses the new capability. A scalar
/// kind can never set positions, so its head is always `-`.
fn values_col(s: &IndexSpec) -> Option<String> {
    if !s.with_positions && s.values.is_empty() {
        return None;
    }
    let head = if s.with_positions { "pos" } else { "-" };
    let mut col = String::from(head);
    for v in &s.values {
        let _ = write!(col, ",{}:{}", esc_field(&v.name), v.ty.tag());
    }
    Some(col)
}

/// A scalar (range / unique) index's line whose 7th column declares its
/// stored `VALUES` — the v5 form. Positions are text-only, so a `pos`
/// head here fails the parse rather than being ignored.
fn scalar_spec(parts: &[&str], version: u8) -> Option<IndexSpec> {
    let (with_positions, values) = parse_text_col(parts[6])?;
    if with_positions || values.is_empty() {
        return None;
    }
    Some(IndexSpec {
        name: unesc(parts[0])?,
        prefix: unesc(parts[1])?,
        fields: col_to_fields(parts[2], version >= 2)?,
        ty: ValType::parse(parts[3].as_bytes())?,
        kind: IndexKind::parse(parts[4].as_bytes())?,
        max_bytes: parts[5].parse().ok()?,
        ann: None,
        group_by: None,
        with_positions: false,
        values,
    })
}

/// The inverse of [`values_col`]. `esc_field` escapes commas, so splitting
/// on one cannot break a field name in half.
fn parse_text_col(col: &str) -> Option<(bool, Vec<ValueSpec>)> {
    let mut parts = col.split(',');
    let pos = match parts.next()? {
        "pos" => true,
        "-" => false,
        _ => return None,
    };
    let values = parts
        .map(|p| {
            let (name, ty) = p.rsplit_once(':')?;
            Some(ValueSpec { name: unesc(name)?, ty: ValType::parse(ty.as_bytes())? })
        })
        .collect::<Option<Vec<_>>>()?;
    Some((pos, values))
}

/// `esc`, plus the two separators the field column introduces. A hash
/// field may legally contain `,` or `:`, and unescaped either one would
/// silently split one field into two -- corrupting the catalog rather
/// than failing to load it.
fn esc_field(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    for &c in b {
        if c == b',' || c == b':' {
            let _ = write!(out, "%{c:02X}");
        } else {
            out.push_str(&esc(&[c]));
        }
    }
    out
}

fn esc(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    for &c in b {
        if c == b'\t' || c == b'\n' || c == b'%' || !(32..127).contains(&c) {
            let _ = write!(out, "%{c:02X}");
        } else {
            out.push(c as char);
        }
    }
    out
}

fn unesc(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

fn fields_to_col(fields: &[FieldSpec]) -> String {
    fields
        .iter()
        .map(|f| format!("{}:{}", esc_field(&f.name), f.weight))
        .collect::<Vec<_>>()
        .join(",")
}

/// Parse sidecar column 3. In v1 it is a single bare escaped name with no
/// weight — which is exactly the neutrally-weighted one-element case, so
/// an old sidecar loads as a one-field multi-field index rather than
/// through a second code path. v2 and v3 both carry `name:weight`.
fn col_to_fields(col: &str, weighted: bool) -> Option<Vec<FieldSpec>> {
    if !weighted {
        return Some(vec![FieldSpec::new(unesc(col)?)]);
    }
    let mut out = Vec::new();
    for part in col.split(',') {
        let (name, weight) = part.rsplit_once(':')?;
        out.push(FieldSpec { name: unesc(name)?, weight: weight.parse().ok()? });
    }
    if out.is_empty() { None } else { Some(out) }
}

#[cfg(test)]
#[path = "catalog_sidecar_tests.rs"]
mod tests;

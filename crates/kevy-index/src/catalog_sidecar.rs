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

use crate::catalog::{AnnSpec, Catalog, FieldSpec, IndexKind, IndexSpec, ValType};

impl Catalog {
    /// Serialize to the sidecar text form (one line per index:
    /// `name<TAB>prefix<TAB>field<TAB>ty<TAB>kind<TAB>max_bytes[<TAB>ann]`,
    /// fields hex-escaped for tabs/newlines via `%XX`; the 7th column
    /// is `dim,distance,m,ef` for ANN kinds).
    pub fn to_sidecar(&self) -> String {
        let mut out = String::from("kevy-index-catalog v2\n");
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
            // escaped group field for Agg.
            if let Some(a) = &s.ann {
                let _ = write!(out, "\t{},{},{},{}", a.dim, a.distance, a.m, a.ef);
            } else if let Some(g) = &s.group_by {
                let _ = write!(out, "\t{}", esc(g));
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
        let v2 = match lines.next()? {
            "kevy-index-catalog v2" => true,
            "kevy-index-catalog v1" => false,
            _ => return None,
        };
        let mut c = Catalog::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            c.create(spec_from_line(line, v2)?).ok()?;
        }
        Some(c)
    }}

fn spec_from_line(line: &str, v2: bool) -> Option<IndexSpec> {
    let parts: Vec<&str> = line.split('\t').collect();
    if !(parts.len() == 6 || parts.len() == 7) {
        return None;
    }
    let kind = IndexKind::parse(parts[4].as_bytes())?;
    let (ann, group_by) = if parts.len() == 7 {
        match kind {
            IndexKind::Ann => {
                let nums: Vec<&str> = parts[6].split(',').collect();
                if nums.len() != 4 {
                    return None;
                }
                (
                    Some(AnnSpec {
                        dim: nums[0].parse().ok()?,
                        distance: nums[1].parse().ok()?,
                        m: nums[2].parse().ok()?,
                        ef: nums[3].parse().ok()?,
                    }),
                    None,
                )
            }
            IndexKind::Agg => (None, Some(unesc(parts[6])?)),
            _ => return None,
        }
    } else {
        (None, None)
    };
    Some(IndexSpec {
        name: unesc(parts[0])?,
        prefix: unesc(parts[1])?,
        fields: col_to_fields(parts[2], v2)?,
        ty: ValType::parse(parts[3].as_bytes())?,
        kind,
        max_bytes: parts[5].parse().ok()?,
        ann,
        group_by,
    })
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
/// through a second code path.
fn col_to_fields(col: &str, v2: bool) -> Option<Vec<FieldSpec>> {
    if !v2 {
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
mod sidecar_v2_tests {
    use super::*;
    use crate::IndexState;

    fn spec(name: &str) -> IndexSpec {
        IndexSpec::single_field(
            name.into(),
            b"user:".to_vec(),
            b"age".to_vec(),
            ValType::I64,
            IndexKind::Range,
        )
    }

    /// The point of the version bump: a sidecar written before
    /// multi-field must keep loading forever. A catalog that refuses to
    /// parse is not an error the operator sees -- it is every index
    /// silently rebuilding from scratch on the next boot.
    #[test]
    fn a_v1_sidecar_still_loads() {
        let v1 = "kevy-index-catalog v1\nidx\tuser:\tage\ti64\trange\t0\n";
        let c = Catalog::from_sidecar(v1).expect("v1 must stay readable");
        let got: Vec<_> = c.iter().collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.field(), b"age");
        assert_eq!(got[0].0.fields.len(), 1, "a v1 field is the one-element case");
        assert_eq!(got[0].0.fields[0].weight, 1.0, "and it is neutrally weighted");
    }

    /// The format must be able to carry weighted multi-field specs
    /// NOW, so that lifting the engine gate later is not also a disk
    /// format change. Only the writing half is checked: `create`
    /// refuses multi-field today, and `from_sidecar` goes through it,
    /// so no such sidecar can exist to read back yet.
    #[test]
    fn v2_serialises_several_weighted_fields() {
        let mut s = spec("multi");
        s.fields = vec![
            FieldSpec { name: b"title".to_vec(), weight: 3.0 },
            FieldSpec { name: b"body".to_vec(), weight: 1.0 },
        ];
        let mut c = Catalog::new();
        c.specs.push((s, IndexState::Building));
        let text = c.to_sidecar();
        assert!(text.starts_with("kevy-index-catalog v2"));
        let col3 = text.lines().nth(1).unwrap().split('\t').nth(2).unwrap();
        assert_eq!(col3, "title:3,body:1");
    }

    /// A field name containing the separators must survive, or a
    /// perfectly legal hash field silently corrupts the catalog.
    #[test]
    fn separators_in_a_field_name_survive() {
        let mut s = spec("esc");
        s.fields = vec![FieldSpec::new(b"we:ird,name\there".to_vec())];
        let mut c = Catalog::new();
        c.create(s).unwrap();
        let back = Catalog::from_sidecar(&c.to_sidecar()).expect("escaped round trip");
        let got: Vec<_> = back.iter().collect();
        assert_eq!(got[0].0.field(), b"we:ird,name\there");
    }

    #[test]
    fn an_unknown_version_is_refused_rather_than_guessed() {
        assert!(Catalog::from_sidecar("kevy-index-catalog v9\n").is_none());
    }
}


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
        let mut out = String::from("kevy-index-catalog v3\n");
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
            } else if s.with_positions {
                out.push_str("\tpos");
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
                    false,
                )
            }
            IndexKind::Agg => (None, Some(unesc(parts[6])?), false),
            // A text index's 7th column is the v3 positions flag; older
            // sidecars never wrote one, so it only appears from v3 on.
            IndexKind::Text if version >= 3 && parts[6] == "pos" => (None, None, true),
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
    fn v3_serialises_several_weighted_fields() {
        let mut s = spec("multi");
        s.fields = vec![
            FieldSpec { name: b"title".to_vec(), weight: 3.0 },
            FieldSpec { name: b"body".to_vec(), weight: 1.0 },
        ];
        let mut c = Catalog::new();
        c.specs.push((s, IndexState::Building));
        let text = c.to_sidecar();
        assert!(text.starts_with("kevy-index-catalog v3"));
        let col3 = text.lines().nth(1).unwrap().split('\t').nth(2).unwrap();
        assert_eq!(col3, "title:3,body:1");
    }

    /// A v2 sidecar written before positions must keep loading — same
    /// forever-readable contract that protects v1.
    #[test]
    fn a_v2_sidecar_still_loads() {
        let v2 = "kevy-index-catalog v2\nidx\tuser:\ttitle:3,body:1\tstr\ttext\t0\n";
        let c = Catalog::from_sidecar(v2).expect("v2 must stay readable");
        let got: Vec<_> = c.iter().collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.fields.len(), 2);
        assert!(!got[0].0.with_positions, "a v2 text index has no positions flag");
    }

    /// A text index created WITH POSITIONS round-trips the flag through
    /// the v3 `pos` column; a text index without it does not, and neither
    /// grows a spurious 7th column that would collide with ann/agg.
    #[test]
    fn positions_flag_round_trips_on_v3() {
        let mut with = IndexSpec::single_field(
            b"phrase".into(),
            b"doc:".to_vec(),
            b"body".to_vec(),
            ValType::Str,
            IndexKind::Text,
        );
        with.with_positions = true;
        let plain = IndexSpec::single_field(
            b"plain".into(),
            b"doc:".to_vec(),
            b"body".to_vec(),
            ValType::Str,
            IndexKind::Text,
        );
        let mut c = Catalog::new();
        c.create(with).unwrap();
        c.create(plain).unwrap();
        let text = c.to_sidecar();
        // The positions index carries a 7th `pos` column; the plain one
        // stays at six.
        let lines: Vec<&str> = text.lines().skip(1).collect();
        let pos_line = lines.iter().find(|l| l.starts_with("phrase")).unwrap();
        let plain_line = lines.iter().find(|l| l.starts_with("plain")).unwrap();
        assert_eq!(pos_line.split('\t').count(), 7);
        assert_eq!(pos_line.split('\t').nth(6), Some("pos"));
        assert_eq!(plain_line.split('\t').count(), 6);

        let back = Catalog::from_sidecar(&text).expect("v3 round trip");
        let specs: Vec<_> = back.iter().map(|(s, _)| s.clone()).collect();
        let with_back = specs.iter().find(|s| s.name == b"phrase").unwrap();
        let plain_back = specs.iter().find(|s| s.name == b"plain").unwrap();
        assert!(with_back.with_positions, "positions flag survives the round trip");
        assert!(!plain_back.with_positions, "plain text index stays position-free");
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


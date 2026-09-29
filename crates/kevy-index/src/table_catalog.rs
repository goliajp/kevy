//! [`TableCatalog`] — the table registry, and the window questions
//! both engine faces ask it.

use crate::spec::IndexSpec;
use crate::table::{MAX_TABLES, TableSpec, WindowSpec};
use crate::table_sidecar::{spec_from_line, spec_to_line};

/// The table registry (mirrors [`crate::Catalog`]): named specs +
/// sidecar text round-trip. Cap [`MAX_TABLES`].
#[derive(Debug, Clone, Default)]
pub struct TableCatalog {
    specs: Vec<TableSpec>,
}

impl TableCatalog {
    /// The WINDOW clause a compiled index named `index_name` serves, if
    /// any, with the shape its tree slides in: a windowed table's
    /// single-column INDEX on the window column, or an ORDERPATH the
    /// window column leads ascending (a DESC lead has no tree-prefix
    /// property and never slides). Shared by both engine faces so the
    /// mapping cannot drift.
    ///
    /// ```
    /// use kevy_index::{TableCatalog, WindowShape, parse_table_declare};
    /// let mut cat = TableCatalog::new();
    /// cat.create(parse_table_declare(&[
    ///     b"TABLE.DECLARE", b"t", b"PREFIX", b"t:", b"PK", b"id", b"COLUMN", b"id", b"i64",
    ///     b"COLUMN", b"at", b"i64", b"INDEX", b"at", b"range",
    ///     b"WINDOW", b"at", b"SPAN", b"100", b"BUCKET", b"10",
    /// ])?)?;
    /// let (w, shape) = cat.window_for(b"t.at").expect("the window column's index slides");
    /// assert_eq!((w.span, shape), (100, WindowShape::PlainI64));
    /// assert!(cat.is_window_driver(b"t.at"));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn window_for(&self, index_name: &[u8]) -> Option<(WindowSpec, crate::WindowShape)> {
        window_for(self, index_name)
    }

    /// Whether `index_name` is its table's row-eviction DRIVER: the one
    /// windowed access path per table that discovers the eviction batch
    /// and seals the rows (every other windowed path only slides its own
    /// tree — two drivers would seal the same batch twice). The
    /// window-column INDEX drives when declared; otherwise the first
    /// ascending-led ORDERPATH does.
    ///
    /// ```
    /// use kevy_index::{TableCatalog, parse_table_declare};
    /// let mut cat = TableCatalog::new();
    /// cat.create(parse_table_declare(&[
    ///     b"TABLE.DECLARE", b"t", b"PREFIX", b"t:", b"PK", b"id", b"COLUMN", b"id", b"i64",
    ///     b"COLUMN", b"at", b"i64", b"INDEX", b"at", b"range",
    /// ])?)?;
    /// assert!(!cat.is_window_driver(b"t.at"), "a table without a window has no driver");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn is_window_driver(&self, index_name: &[u8]) -> bool {
        window_driver(self, index_name)
    }

    /// Whether a compiled TEXT index belongs to a windowed table — its
    /// documents freeze into cold bucket segments as the window slides.
    /// (The batch discovery lives on the table's window driver; the text
    /// index only needs a cold directory.) Shared by both engine faces.
    ///
    /// ```
    /// use kevy_index::{IndexKind, IndexSpec, TableCatalog, ValType};
    /// let text = IndexSpec::builder("t.body", "t:", IndexKind::Text, ValType::Str).with_field("body").build()?;
    /// assert!(!TableCatalog::new().is_windowed_text(&text), "no table t is declared");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn is_windowed_text(&self, spec: &IndexSpec) -> bool {
        window_text_for(self, spec)
    }

    /// Empty catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register; errors on duplicate / cap / structure.
    pub fn create(&mut self, spec: TableSpec) -> Result<(), crate::CatalogError> {
        spec.validate()?;
        if self.specs.len() >= MAX_TABLES {
            return Err(crate::CatalogError::Full(crate::Declared::Table));
        }
        if self.specs.iter().any(|s| s.name == spec.name) {
            return Err(crate::CatalogError::Exists(crate::Declared::Table));
        }
        self.specs.push(spec);
        Ok(())
    }

    /// Drop by name; `false` if absent.
    pub fn drop_table(&mut self, name: &[u8]) -> bool {
        let n = self.specs.len();
        self.specs.retain(|s| s.name != name);
        self.specs.len() != n
    }

    /// Lookup.
    pub fn get(&self, name: &[u8]) -> Option<&TableSpec> {
        self.specs.iter().find(|s| s.name == name)
    }

    /// Declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &TableSpec> {
        self.specs.iter()
    }

    /// Count.
    pub fn len(&self) -> usize {
        self.specs.len()
    }

    /// Empty?
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// Sidecar text (one line per table) — same lifecycle genre as the
    /// index/view catalogs.
    pub fn to_sidecar(&self) -> String {
        let mut out = String::from("kevy-table-catalog v1\n");
        for s in &self.specs {
            out.push_str(&spec_to_line(s));
            out.push('\n');
        }
        out
    }

    /// Parse the sidecar text; `None` on malformed input. Every line
    /// re-validates — a spec the validator refuses cannot be smuggled
    /// in through a hand-edited sidecar.
    pub fn from_sidecar(text: &str) -> Option<TableCatalog> {
        let mut lines = text.lines();
        if lines.next()? != "kevy-table-catalog v1" {
            return None;
        }
        let mut c = TableCatalog::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            c.create(spec_from_line(line)?).ok()?;
        }
        Some(c)
    }
}

pub(crate) fn window_for(
    cat: &TableCatalog,
    index_name: &[u8],
) -> Option<(WindowSpec, crate::WindowShape)> {
    let dot = index_name.iter().position(|&b| b == b'.')?;
    let (tname, suffix) = (&index_name[..dot], &index_name[dot + 1..]);
    let t = cat.get(tname)?;
    let w = t.window.clone()?;
    if suffix == w.column {
        return Some((w, crate::WindowShape::PlainI64));
    }
    let leads = t.orderpaths.iter().any(|op| op.name == suffix && op.led_ascending_by(&w.column));
    leads.then_some((w, crate::WindowShape::CompositeLed))
}

pub(crate) fn window_text_for(cat: &TableCatalog, spec: &IndexSpec) -> bool {
    if spec.kind() != crate::IndexKind::Text {
        return false;
    }
    let Some(dot) = spec.name().iter().position(|&b| b == b'.') else { return false };
    cat.get(&spec.name()[..dot]).is_some_and(|t| t.window.is_some())
}

pub(crate) fn window_driver(cat: &TableCatalog, index_name: &[u8]) -> bool {
    let Some(dot) = index_name.iter().position(|&b| b == b'.') else { return false };
    let (tname, suffix) = (&index_name[..dot], &index_name[dot + 1..]);
    let Some(t) = cat.get(tname) else { return false };
    let Some(w) = &t.window else { return false };
    if t.indexes.iter().any(|ix| ix.column == w.column) {
        return suffix == w.column;
    }
    t.orderpaths
        .iter()
        .find(|op| op.led_ascending_by(&w.column))
        .is_some_and(|op| op.name == suffix)
}

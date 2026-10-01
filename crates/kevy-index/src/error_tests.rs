//! Every refusal's wire text, display text and cause chain.

use std::error::Error as _;

use super::*;
use crate::{AnnSpec, IndexKind, IndexSpec, ValType, WhereError, WindowBound};

const SPEC_ERRORS: [(SpecError, &str); 17] = [
    (SpecError::NoFields, "ERR index needs at least one field"),
    (SpecError::SeveralFieldsNotText, "ERR only KIND text indexes several fields"),
    (SpecError::PositionsNotText, "ERR WITH POSITIONS requires KIND text"),
    (SpecError::ValuesKind, "ERR VALUES requires KIND text|range|unique"),
    (SpecError::AnnNeedsVector, "ERR KIND ann requires TYPE vector and DIM"),
    (SpecError::VectorNeedsAnn, "ERR TYPE vector requires KIND ann"),
    (SpecError::AnnParamsNeedAnn, "ERR ANN parameters require KIND ann"),
    (SpecError::AggNeedsGroupBy, "ERR KIND agg requires GROUPBY <field>"),
    (SpecError::AggNeedsNumber, "ERR KIND agg requires TYPE i64|f64"),
    (SpecError::GroupByNeedsAgg, "ERR GROUPBY requires KIND agg"),
    (SpecError::CompositeNeedsRange, "ERR COMPOSITE requires KIND range"),
    (SpecError::CompositeNeedsStr, "ERR COMPOSITE requires TYPE str"),
    (SpecError::CompositeWithValues, "ERR COMPOSITE cannot combine with VALUES"),
    (SpecError::CompositeFieldCount, "ERR COMPOSITE declares exactly one FIELD"),
    (SpecError::CompositeNoColumns, "ERR COMPOSITE needs at least one column"),
    (SpecError::CompositeTooManyColumns, "ERR COMPOSITE supports at most 8 columns"),
    (SpecError::CompositeColumnType, "ERR COMPOSITE columns must be i64|f64|str"),
];

#[test]
fn a_spec_error_displays_its_wire_text_without_the_code() {
    for (e, wire) in SPEC_ERRORS {
        assert_eq!(e.as_wire(), wire);
        assert_eq!(e.to_string(), &wire["ERR ".len()..]);
        assert!(e.source().is_none(), "{e} has no cause");
    }
}

#[test]
fn text_without_a_code_reads_as_itself() {
    assert_eq!(without_code("ERR a b"), "a b");
    assert_eq!(without_code("bare"), "bare");
}

#[test]
fn a_view_error_displays_its_wire_text_without_the_code() {
    for (e, text) in [
        (ViewError::TooDeep, "view tree deeper than 3"),
        (ViewError::TooManyLeaves, "view tree has more than 4 leaves"),
    ] {
        assert_eq!(e.as_wire(), format!("ERR {text}"));
        assert_eq!(e.to_string(), text);
    }
}

#[test]
fn each_declared_kind_names_itself_and_its_cap() {
    for (d, noun) in
        [(Declared::Index, "index"), (Declared::View, "view"), (Declared::Table, "table")]
    {
        assert_eq!(d.as_str(), noun);
        assert_eq!(CatalogError::Full(d).to_wire(), format!("ERR {noun} limit reached (64)"));
        assert_eq!(CatalogError::Exists(d).to_string(), format!("{noun} already exists"));
    }
}

#[test]
fn a_catalog_error_words_every_refusal_and_keeps_its_cause() {
    let global = CatalogError::GlobalNeedsOrder;
    assert_eq!(global.to_wire(), "ERR PARTITION global requires KIND range|unique");
    assert!(global.source().is_none());
    let splits = CatalogError::SplitsOutOfOrder;
    assert_eq!(splits.to_string(), "SPLIT AT values must be strictly increasing");
    assert!(splits.source().is_none());

    let view = CatalogError::from(ViewError::TooManyLeaves);
    assert_eq!(view, CatalogError::View(ViewError::TooManyLeaves));
    assert_eq!(view.to_wire(), "ERR view tree has more than 4 leaves");
    assert_eq!(view.source().map(|c| c.to_string()), Some(ViewError::TooManyLeaves.to_string()));

    let table = CatalogError::from(TableError::EmptyName);
    assert_eq!(table, CatalogError::Table(TableError::EmptyName));
    assert_eq!(table.to_wire(), "ERR table name must be non-empty");
    assert_eq!(table.source().map(|c| c.to_string()), Some("table name must be non-empty".into()));
}

#[test]
fn a_table_error_words_each_refusal() {
    let col = || b"at".to_vec();
    for (e, text) in [
        (TableError::SplitAtEmpty, "SPLIT AT needs at least one value"),
        (TableError::DuplicateAutodeclare, "duplicate AUTODECLARE clause"),
        (TableError::AutodeclareNotPositive, "AUTODECLARE needs a positive integer"),
        (
            TableError::GlobalNotHere,
            "GLOBAL is a server feature; an embedded store's paths are local",
        ),
        (TableError::EmptyName, "table name must be non-empty"),
        (TableError::NoColumns, "a table needs at least one COLUMN"),
        (TableError::OrderpathTooManyColumns, "ORDERPATH supports at most 8 columns"),
        (TableError::ColumnUndeclared(col()), "column 'at' is not declared"),
        (TableError::WindowNotInteger(WindowBound::Bucket), "WINDOW BUCKET must be an integer"),
        (TableError::WindowNotInteger(WindowBound::Span), "WINDOW SPAN must be an integer"),
    ] {
        assert_eq!(e.to_wire(), format!("ERR {text}"));
        assert!(e.source().is_none(), "{e} has no cause");
    }
}

#[test]
fn a_table_error_from_a_spec_error_keeps_it_as_the_cause() {
    let e = TableError::from(SpecError::AggNeedsNumber);
    assert_eq!(e, TableError::Spec(SpecError::AggNeedsNumber));
    assert_eq!(e.to_wire(), "ERR KIND agg requires TYPE i64|f64");
    let cause = e.source().expect("the spec error is the cause").to_string();
    assert_eq!(cause, SpecError::AggNeedsNumber.to_string());
    let outer = CatalogError::from(e);
    assert_eq!(outer.source().and_then(|c| c.source()).map(|c| c.to_string()), Some(cause));
}

#[test]
fn a_where_error_names_the_bound_and_every_declared_column() {
    let bad_time = WhereError::TimeExpression { bound: b"@soon".to_vec(), column: b"at".to_vec() };
    assert_eq!(bad_time.to_string(), "WHERE bound '@soon' is not a valid time expression for 'at'");
    let unknown = WhereError::UnknownColumn {
        column: b"zip".to_vec(),
        declared: vec![b"city".to_vec(), b"at".to_vec(), b"id".to_vec()],
    };
    assert_eq!(
        unknown.to_string(),
        "WHERE names column 'zip', which this composite does not declare — it declares: city, at, id"
    );
    let order = WhereError::NotLeadingPrefix { declared: vec![b"a".to_vec(), b"b".to_vec()] };
    assert_eq!(
        order.to_string(),
        "WHERE columns must be a leading prefix of the composite's declared order (a, b)"
    );
}

/// A sink that takes `room` bytes, then refuses every write.
struct Capped {
    room: usize,
    got: String,
}

impl fmt::Write for Capped {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if s.len() > self.room {
            return Err(fmt::Error);
        }
        self.room -= s.len();
        self.got.push_str(s);
        Ok(())
    }
}

#[test]
fn a_column_list_stops_at_the_first_write_its_sink_refuses() {
    use std::fmt::Write as _;
    let e = WhereError::NotLeadingPrefix { declared: vec![b"ab".to_vec(), b"cd".to_vec()] };
    let whole = e.to_string();
    let head = whole.find("ab").unwrap();
    // refused on a name, then on the separator after the first name
    for (room, kept) in [(head, &whole[..head]), (head + 2, &whole[..head + 2])] {
        let mut sink = Capped { room, got: String::new() };
        assert_eq!(write!(sink, "{e}"), Err(fmt::Error));
        assert_eq!(sink.got, kept);
    }
}

#[test]
fn a_builder_refuses_kind_parts_that_disagree() {
    let b = |kind, ty| IndexSpec::builder("n", "p:", kind, ty).with_field("f");
    let cases = [
        (b(IndexKind::Ann, ValType::Vector).build(), SpecError::AnnNeedsVector),
        (b(IndexKind::Range, ValType::Vector).build(), SpecError::VectorNeedsAnn),
        (
            b(IndexKind::Range, ValType::I64).with_ann(AnnSpec::new(3)).build(),
            SpecError::AnnParamsNeedAnn,
        ),
        (b(IndexKind::Agg, ValType::I64).build(), SpecError::AggNeedsGroupBy),
        (b(IndexKind::Agg, ValType::Str).with_group_by("g").build(), SpecError::AggNeedsNumber),
        (b(IndexKind::Range, ValType::I64).with_group_by("g").build(), SpecError::GroupByNeedsAgg),
    ];
    for (got, want) in cases {
        assert_eq!(got.err(), Some(want));
    }
    let agg = b(IndexKind::Agg, ValType::F64).with_group_by("g").build();
    assert!(agg.is_ok(), "{agg:?}");
}

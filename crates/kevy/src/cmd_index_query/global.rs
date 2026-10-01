//! Reads over a global index. Its entries live in value-ordered
//! partitions, one per owning shard, and the rows they point at live on
//! other shards. So a read goes only to the partitions its range meets,
//! a plain page walks them in order — one partition per phase, the pages
//! concatenated at the origin rather than merged — and `FIELDS` answers
//! from the stored `VALUES`, the only copy of a row's fields the owner
//! holds.
//!
//! The continuation phase is `IDX.PART <p> <want> <carried> <argv…>`:
//! partition `p` is asked for `want` more hits of the original query,
//! and `carried` holds the page so far (a plain hit chunk), so the
//! runtime keeps no state between phases.

use kevy_index::{Partitioning, Segment, partition_owner};
use kevy_store::Store;

use super::args::{Query, Shape};
use super::wire::HydrationRow;
use super::{Hydrated, ST_BADARGS};
use crate::state::{CatalogState, Ctx};

/// The continuation verb, internal to the fan-out.
pub(crate) const PART_VERB: &[u8] = b"IDX.PART";
/// Where the original argv starts in an `IDX.PART` argv.
pub(crate) const PART_ORIG: usize = 4;

/// Whether `name` is a global index.
pub(super) fn is_global(ctx: &Ctx<'_>, name: &[u8]) -> bool {
    ctx.state.catalogs.index().is_some_and(|c| c.partitioning(name).is_global())
}

/// The partitions a read over a global index meets, in value order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Walk {
    /// The partition holding the range's low end.
    pub(crate) first: usize,
    /// The partition holding its high end.
    pub(crate) last: usize,
    /// Where a page starts: the partition holding the cursor, or `first`.
    pub(crate) start: usize,
    /// The page size asked for.
    pub(crate) limit: usize,
    /// A page in the driving `(value, key)` order — no selection clause —
    /// which the partitions can answer one after another.
    pub(crate) in_order: bool,
}

/// The walk for an `IDX.QUERY` / `IDX.COUNT` / `IDX.EXPLAIN` argv on a
/// global index; `None` for a local index or an argv the shards will
/// refuse themselves.
pub(crate) fn walk(catalogs: &CatalogState, argv: &[Vec<u8>]) -> Option<(Walk, Partitioning)> {
    let cat = catalogs.index()?;
    let name = argv.get(1)?;
    let part = cat.partitioning(name);
    if !part.is_global() {
        return None;
    }
    let (spec, _) = cat.get(name)?;
    let q = Query::parse(argv)?;
    if matches!(q.shape, Shape::Verify) {
        return None;
    }
    let now = (kevy_store::now_unix_ms() / 1000) as i64;
    let (min, max) = q.bounds_for(spec, now).ok()?;
    let first = part.partition_of(&min.order_bytes());
    let last = part.partition_of(&max.order_bytes()).max(first);
    let at_cursor = q.cursor(spec.ty()).map(|c| part.partition_of(&c.value.order_bytes()));
    let start = at_cursor.map_or(first, |p| p.clamp(first, last));
    let walk = Walk { first, last, start, limit: q.limit, in_order: !q.selects() };
    Some((walk, part.clone()))
}

/// The shards an extension argv needs (`Commands::extension_targets`):
/// a page in order goes to the partition it starts in, a continuation to
/// the partition it names, anything else to every partition the range
/// meets. `None` = every shard.
pub(crate) fn targets(catalogs: &CatalogState, n: usize, argv: &[Vec<u8>]) -> Option<Vec<usize>> {
    let verb = argv.first()?;
    if verb.eq_ignore_ascii_case(PART_VERB) {
        let p = parse_usize(argv.get(1)?)?;
        return Some(vec![partition_owner(argv.get(PART_ORIG + 1)?, p, n)]);
    }
    let reads = [b"IDX.QUERY".as_slice(), b"IDX.COUNT", b"IDX.EXPLAIN"];
    if !reads.iter().any(|r| verb.eq_ignore_ascii_case(r)) {
        return None;
    }
    let (w, _) = walk(catalogs, argv)?;
    let name = &argv[1];
    if w.in_order && verb.eq_ignore_ascii_case(b"IDX.QUERY") {
        return Some(vec![partition_owner(name, w.start, n)]);
    }
    Some((w.first..=w.last).map(|p| partition_owner(name, p, n)).collect())
}

/// The owner's half of `IDX.PART`: the original query against this
/// shard's partition, asking only for the hits the page still wants.
pub(super) fn op_part(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let (Some(want), Some(orig)) =
        (argv.get(2).and_then(|w| parse_usize(w)), argv.get(PART_ORIG..))
    else {
        return vec![ST_BADARGS];
    };
    let Some(mut q) = Query::parse(orig) else {
        return vec![ST_BADARGS];
    };
    q.limit = want;
    super::query::run_parsed(ctx, store, &q, b"IDX.QUERY")
}

/// The tag after the status byte of a global index's `IDX.REBUILD` chunk.
pub(crate) const REBUILD_TAG: u8 = b'g';

/// A shard's half of `IDX.REBUILD` on a global index: its rows' values in
/// rank buckets, `[ST_OK][REBUILD_TAG]` and the points, from which the
/// origin takes new split points.
pub(super) fn op_rebuild(ctx: &Ctx<'_>, store: &mut Store, name: &[u8]) -> Vec<u8> {
    let Some(spec) = ctx.state.catalogs.index().and_then(|c| c.get(name).map(|(s, _)| s.clone()))
    else {
        return vec![super::ST_NOINDEX];
    };
    let q = crate::index_runtime::POINTS_PER_PARTITION * ctx.state.nshards().max(1);
    let mut chunk = vec![super::ST_OK, REBUILD_TAG];
    crate::index_runtime::put_points(
        &mut chunk,
        &crate::index_runtime::quantile_points(store, &spec, q),
    );
    chunk
}

/// Each `FIELDS` name's position among the stored `VALUES`, or the
/// refusal naming the first one not stored (and what is): the owner
/// holds the partition, not the rows, so it cannot read a row's hash.
pub(super) fn stored_positions(
    spec: &kevy_index::IndexSpec,
    fields: &[Vec<u8>],
) -> Result<Vec<usize>, Vec<u8>> {
    fields
        .iter()
        .map(|f| {
            spec.values().iter().position(|v| v.name == *f).ok_or_else(|| {
                let stored: Vec<&[u8]> = spec.values().iter().map(|v| v.name.as_slice()).collect();
                super::ops_clauses::nofield_error("FIELDS on a global index", f, &stored)
            })
        })
        .collect()
}

/// [`stored_rows`] for `fields` by name, or the refusal naming the first
/// one not stored.
pub(super) fn stored_page(
    spec: &kevy_index::IndexSpec,
    seg: &Segment,
    hits: &[(&kevy_index::IndexValue, &[u8])],
    fields: &[Vec<u8>],
) -> Result<Vec<HydrationRow>, Vec<u8>> {
    Ok(stored_rows(seg, hits, &stored_positions(spec, fields)?))
}

/// The hydration rows for `hits` (each a value and the key held under
/// it), read from the partition's stored values.
pub(super) fn stored_rows(
    seg: &Segment,
    hits: &[(&kevy_index::IndexValue, &[u8])],
    positions: &[usize],
) -> Vec<HydrationRow> {
    let row = |(v, k): &(&kevy_index::IndexValue, &[u8])| -> Hydrated {
        let all = seg.stored_row(v, k);
        positions.iter().map(|&p| all.get(p).cloned().flatten()).collect()
    };
    hits.iter().map(|h| Ok(Some(row(h)))).collect()
}

fn parse_usize(b: &[u8]) -> Option<usize> {
    std::str::from_utf8(b).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use kevy_index::{Catalog, IndexKind, IndexSpec, ValType, order_key};

    use super::*;

    /// A catalog holding `g`, global over 0..1000 with `n` partitions (one
    /// per shard), and `l`, local, over the same field.
    fn catalogs(n: usize) -> crate::RuntimeState {
        let spec = |name: &[u8]| {
            IndexSpec::builder(name.to_vec(), b"u:".to_vec(), IndexKind::Range, ValType::I64)
                .with_field(b"age".to_vec())
                .build()
                .unwrap()
        };
        let splits =
            (1..n).map(|k| order_key(ValType::I64, (k * 1000 / n).to_string().as_bytes()).unwrap());
        let mut cat = Catalog::default();
        cat.create_with(spec(b"g"), Partitioning::Global { splits: splits.collect() }).unwrap();
        cat.create(spec(b"l")).unwrap();
        let cfg = std::sync::Arc::new(kevy_config::Config::default());
        let state = crate::RuntimeState::new(cfg, std::path::PathBuf::new(), n).unwrap();
        state.install_index_catalog(cat);
        state
    }

    fn argv(words: &str) -> Vec<Vec<u8>> {
        words.split(' ').map(|w| w.as_bytes().to_vec()).collect()
    }

    /// The owner of the partition `v` falls in.
    fn owner(n: usize, v: usize) -> usize {
        partition_owner(b"g", (v * n / 1000).min(n - 1), n)
    }

    #[test]
    fn a_read_names_only_the_owners_of_the_partitions_it_meets() {
        for n in [1, 4, 8, 16, 64] {
            let c = catalogs(n);
            let t = |words: &str| targets(&c.catalogs, n, &argv(words));
            assert_eq!(t("IDX.QUERY g EQ 999"), Some(vec![owner(n, 999)]), "N={n}");
            assert_eq!(t("IDX.QUERY g RANGE 0 0 LIMIT 5"), Some(vec![owner(n, 0)]), "N={n}");
            // a page in order starts in one partition however far it runs
            assert_eq!(t("IDX.QUERY g RANGE 0 999"), Some(vec![owner(n, 0)]), "N={n}");
            // a count, and a selection, go to each partition met, once
            let all: Vec<usize> = (0..n).map(|p| partition_owner(b"g", p, n)).collect();
            assert_eq!(t("IDX.COUNT g RANGE 0 999"), Some(all.clone()), "N={n}");
            let mut sorted = t("IDX.QUERY g RANGE 0 999 SORT age DESC").unwrap();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..n).collect::<Vec<_>>(), "N={n}: P = N, every shard owns one");
            // the local index keeps the full fan-out
            assert_eq!(t("IDX.QUERY l RANGE 0 999"), None);
            assert_eq!(t("IDX.COUNT l RANGE 0 999"), None);
        }
    }

    #[test]
    fn a_continuation_goes_to_the_partition_it_names() {
        let c = catalogs(8);
        let t = |words: &str| targets(&c.catalogs, 8, &argv(words));
        for p in 0..8 {
            let words = format!("IDX.PART {p} 5 - IDX.QUERY g RANGE 0 999");
            assert_eq!(t(&words), Some(vec![partition_owner(b"g", p, 8)]));
        }
    }

    #[test]
    fn a_walk_starts_where_its_cursor_is() {
        let c = catalogs(4);
        let mut payload = Vec::new();
        super::super::wire::encode_value(&mut payload, &kevy_index::IndexValue::I64(600));
        payload.extend_from_slice(b"u:1");
        let cursor = super::super::wire::hex(&payload);
        let words =
            format!("IDX.QUERY g RANGE 0 999 CURSOR {}", String::from_utf8(cursor).unwrap());
        let (w, _) = walk(&c.catalogs, &argv(&words)).unwrap();
        assert_eq!((w.first, w.start, w.last), (0, 2, 3));
        let (w, _) = walk(&c.catalogs, &argv("IDX.QUERY g RANGE 300 200")).unwrap();
        assert_eq!((w.first, w.last), (1, 1), "an empty range still names one partition");
    }

    #[test]
    fn an_argv_the_shards_refuse_themselves_goes_to_every_shard() {
        let c = catalogs(4);
        let t = |a: &[Vec<u8>]| targets(&c.catalogs, 4, a);
        assert_eq!(t(&[]), None);
        for words in ["IDX.PART", "IDX.PART x - - IDX.QUERY g", "IDX.PART 1", "IDX.QUERY"] {
            assert_eq!(t(&argv(words)), None, "{words}");
        }
        assert_eq!(t(&[b"IDX.PART".to_vec(), b"\xff".to_vec()]), None);
        for words in ["IDX.QUERY g BOGUS", "IDX.QUERY g RANGE x 9", "IDX.VERIFY g"] {
            assert_eq!(walk(&c.catalogs, &argv(words)), None, "{words}");
        }
    }

    #[test]
    fn a_continuation_or_rebuild_the_owner_cannot_read_is_refused() {
        let kevy = crate::KevyCommands::with_state(std::sync::Arc::new(catalogs(1)));
        let mut store = Store::new();
        for words in ["IDX.PART 0 many - IDX.QUERY g RANGE 0 9", "IDX.PART 0 5 - IDX.QUERY g BOGUS"]
        {
            assert_eq!(op_part(&kevy.ctx(), &mut store, &argv(words)), [ST_BADARGS], "{words}");
        }
        assert_eq!(op_rebuild(&kevy.ctx(), &mut store, b"nosuch"), [super::super::ST_NOINDEX]);
    }
}

//! The two-phase form of a catalog verb that samples a global index's
//! split points: `IDX.CREATE … PARTITION global` without `SPLIT`, and a
//! `TABLE.DECLARE` / `ENSURE` / `REPLACE` naming a path `GLOBAL` without
//! `SPLIT AT`. Every shard sends its rows' values cut into rank buckets
//! (the first phase), and the origin runs the verb with split points taken
//! from all of them — so the partitions start even whatever share of the
//! rows the origin holds.
//!
//! Chunk: `[status][tier-blocked u8][paths u16]`, then per path
//! `name_len u16 | name | points` (points as `index_runtime::put_points`
//! writes them).

use std::collections::HashMap;

use kevy_index::{IndexSpec, parse_table_declare_partitioned};
use kevy_resp::{Argv, ArgvView};
use kevy_store::Store;

use crate::cmd_index_install::Sampler;
use crate::index_runtime::{POINTS_PER_PARTITION, put_points, quantile_points, read_points};
use crate::state::Ctx;

/// The verbs with a two-phase form, upper-case.
const VERBS: [&[u8]; 4] = [b"IDX.CREATE", b"TABLE.DECLARE", b"TABLE.ENSURE", b"TABLE.REPLACE"];

/// Whether `argv` is one of [`VERBS`], whatever its case.
pub(crate) fn is_verb(verb: &[u8]) -> bool {
    VERBS.iter().any(|v| verb.eq_ignore_ascii_case(v))
}

/// Whether this catalog verb samples split points, so it takes the
/// two-phase form (the router's question; `upper` is the upper-case verb).
pub(crate) fn samples<A: ArgvView + ?Sized>(upper: &[u8], args: &A) -> bool {
    let argv: Vec<&[u8]> = (0..args.len()).filter_map(|i| args.get(i)).collect();
    !sampled_paths(upper, &argv).is_empty()
}

/// The specs of the paths `argv` declares global without split points; a
/// malformed argv has none (the second phase reports its error).
fn sampled_paths(upper: &[u8], argv: &[&[u8]]) -> Vec<IndexSpec> {
    if upper == b"IDX.CREATE" {
        let args = Argv::from(argv.iter().map(|a| a.to_vec()).collect::<Vec<_>>());
        return match crate::cmd_index::parse_create(&args, &mut Vec::new()) {
            Some((spec, part)) if part.global && part.split.is_empty() => vec![spec],
            _ => Vec::new(),
        };
    }
    let Ok((spec, globals)) = parse_table_declare_partitioned(argv) else { return Vec::new() };
    let wanted =
        |s: &IndexSpec| globals.iter().any(|g| g.path == s.name() && g.split_at.is_empty());
    spec.compile().map(|c| c.into_iter().filter(wanted).collect()).unwrap_or_default()
}

/// A shard's first phase: its rank buckets of each path, and whether its
/// tiering floor refuses a new index. A table that already exists is not
/// sampled — `DECLARE` refuses it and `ENSURE` does not rebuild it.
pub(crate) fn op(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let refs: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
    let upper = argv[0].to_ascii_uppercase();
    let mut paths = sampled_paths(&upper, &refs);
    let keeps = upper == b"TABLE.DECLARE" || upper == b"TABLE.ENSURE";
    let exists = |t: &[u8]| ctx.state.catalogs.table().is_some_and(|c| c.get(t).is_some());
    if keeps && argv.get(1).is_some_and(|t| exists(t)) {
        paths.clear();
    }
    let mut chunk =
        vec![crate::cmd_index_query::ST_OK, u8::from(store.tier_index_floor_blocked(0))];
    chunk.extend_from_slice(&(paths.len() as u16).to_le_bytes());
    for spec in &paths {
        chunk.extend_from_slice(&(spec.name().len() as u16).to_le_bytes());
        chunk.extend_from_slice(&spec.name());
        let parts = ctx.state.nshards().max(1);
        put_points(&mut chunk, &quantile_points(store, spec, POINTS_PER_PARTITION * parts));
    }
    chunk
}

/// The origin's second phase: run the verb with every shard's points.
pub(crate) fn reduce(ctx: &Ctx<'_>, argv: &[Vec<u8>], chunks: &[Vec<u8>]) -> Vec<u8> {
    let (mut samples, mut tier_blocked) = (HashMap::new(), false);
    for c in chunks {
        let Some(blocked) = read_chunk(c, &mut samples) else {
            return b"-ERR a shard's sample did not arrive whole\r\n".to_vec();
        };
        tier_blocked |= blocked;
    }
    let mut sampler = Sampler::Gathered { samples: &samples, tier_blocked };
    let args = Argv::from(argv.to_vec());
    let mut out = Vec::new();
    match argv[0].to_ascii_uppercase().as_slice() {
        b"IDX.CREATE" => crate::cmd_index_install::create(ctx, &mut sampler, &args, &mut out),
        b"TABLE.DECLARE" => crate::cmd_table::cmd_table_declare(ctx, &mut sampler, &args, &mut out),
        b"TABLE.ENSURE" => crate::cmd_table::cmd_table_ensure(ctx, &mut sampler, &args, &mut out),
        _ => crate::cmd_table::cmd_table_replace(ctx, &mut sampler, &args, &mut out),
    }
    out
}

/// Fold one chunk's points into `samples`; its tier flag, or `None` for a
/// chunk this module did not write.
fn read_chunk(c: &[u8], samples: &mut HashMap<Vec<u8>, Vec<(Vec<u8>, u64)>>) -> Option<bool> {
    let blocked = *c.get(1)? != 0;
    let paths = u16::from_le_bytes(c.get(2..4)?.try_into().ok()?);
    let mut pos = 4;
    for _ in 0..paths {
        let len = usize::from(u16::from_le_bytes(c.get(pos..pos + 2)?.try_into().ok()?));
        let name = c.get(pos + 2..pos + 2 + len)?.to_vec();
        pos += 2 + len;
        samples.entry(name).or_default().extend(read_points(c, &mut pos)?);
    }
    Some(blocked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<&[u8]> {
        line.split(' ').map(str::as_bytes).collect()
    }

    fn paths(line: &str) -> Vec<Vec<u8>> {
        let argv = words(line);
        let upper = argv[0].to_ascii_uppercase();
        sampled_paths(&upper, &argv).into_iter().map(|s| s.name().to_vec()).collect()
    }

    #[test]
    fn only_a_global_path_without_split_points_is_sampled() {
        let create = "IDX.CREATE g ON PREFIX u: FIELD age TYPE i64 KIND range";
        assert_eq!(paths(&format!("{create} PARTITION global")), [b"g".to_vec()]);
        assert!(paths(&format!("{create} PARTITION global SPLIT 5")).is_empty());
        assert!(paths(create).is_empty());
        assert!(paths("IDX.CREATE g ON PREFIX").is_empty(), "malformed: the second phase says why");
        let table = "TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 COLUMN a i64 COLUMN b i64";
        let both = format!("{table} INDEX a range GLOBAL INDEX b range GLOBAL SPLIT AT 3");
        assert_eq!(paths(&both), [b"t.a".to_vec()]);
        assert_eq!(paths(&both.replacen("DECLARE", "ENSURE", 1)), [b"t.a".to_vec()]);
        assert!(paths(&format!("{table} INDEX a range")).is_empty());
    }

    fn chunk(blocked: bool, paths: &[(&[u8], &[&[u8]])]) -> Vec<u8> {
        let mut c = vec![0, u8::from(blocked)];
        c.extend_from_slice(&(paths.len() as u16).to_le_bytes());
        for (name, values) in paths {
            c.extend_from_slice(&(name.len() as u16).to_le_bytes());
            c.extend_from_slice(name);
            let pts: Vec<(Vec<u8>, u64)> = values.iter().map(|v| (v.to_vec(), 1)).collect();
            put_points(&mut c, &pts);
        }
        c
    }

    #[test]
    fn every_shards_sample_lands_under_its_path() {
        let mut samples = HashMap::new();
        let a = chunk(false, &[(b"t.a", &[b"1", b"2"]), (b"t.b", &[b"9"])]);
        let b = chunk(true, &[(b"t.a", &[b"3"])]);
        assert_eq!(read_chunk(&a, &mut samples), Some(false));
        assert_eq!(read_chunk(&b, &mut samples), Some(true));
        let one = |v: &[u8]| (v.to_vec(), 1u64);
        assert_eq!(samples[&b"t.a".to_vec()], [one(b"1"), one(b"2"), one(b"3")]);
        assert_eq!(samples[&b"t.b".to_vec()], [one(b"9")]);
        assert_eq!(read_chunk(&a[..a.len() - 1], &mut samples), None, "a cut chunk is refused");
    }

    #[test]
    fn a_shard_whose_tier_floor_is_full_refuses_the_index_for_all() {
        let cfg = std::sync::Arc::new(kevy_config::Config::default());
        let state = crate::RuntimeState::new(cfg, std::path::PathBuf::new(), 2).unwrap();
        let kevy = crate::KevyCommands::with_state(std::sync::Arc::new(state));
        let argv: Vec<Vec<u8>> =
            words("IDX.CREATE g ON PREFIX u: FIELD age TYPE i64 KIND range PARTITION global")
                .into_iter()
                .map(<[u8]>::to_vec)
                .collect();
        let (open, full) = (chunk(false, &[(b"g", &[b"1"])]), chunk(true, &[(b"g", &[b"2"])]));
        let refused = reduce(&kevy.ctx(), &argv, &[open.clone(), full]);
        assert!(String::from_utf8_lossy(&refused).contains("index memory floor"));
        assert!(kevy.state().catalogs.index().is_none_or(|c| c.get(b"g").is_none()));
        assert_eq!(reduce(&kevy.ctx(), &argv, &[open.clone(), open]), b"+OK\r\n");
    }
}

//! Verb documentation metadata — the single source of truth for
//! COMMAND DOCS, llms.txt, and the MCP schema.
//! Parity tests in tests_verb_meta.rs hold this table and
//! the dispatch surface bidirectionally equal.
//!
//! Flag discipline: `write` / `readonly` mirror
//! `kevy_resp::ops_table::OP_TABLE`'s `write` column **literally** for
//! every verb that has a row there (including its documented quirks:
//! RENAME/RENAMENX classify as reads because the server routes them at
//! the runtime Op level, BLPOP/BRPOP classify as reads because the
//! blocked-serve path logs the LPOP/RPOP effect, and IDX./VIEW. catalog
//! mutations are sidecar-persisted — not data writes). Verbs without an
//! OP_TABLE row (connection/admin/tx/pubsub/script/_RO twins) are
//! classified here directly.
//!
//! Arity is Redis semantics: positive = exact argc including the verb
//! itself; negative = at least |n|. Values are derived from each
//! handler's own argc check, not from Redis docs.

/// One verb's documentation row (semantic classification lives in
/// kevy_resp::ops_table::OP_TABLE — this table is the DOC face).
#[derive(Debug, Clone, Copy)]
pub struct VerbMeta {
    /// Canonical uppercase name, as `COMMAND DOCS` reports it and as the
    /// dispatch tables spell it.
    pub name: &'static str,
    /// Which family the reference groups it under — `string`, `index`,
    /// `server` and so on.
    pub group: &'static str,
    /// Redis's arity convention: positive is an exact argument count
    /// INCLUDING the verb, negative is a minimum. `-4` means "at least
    /// four". This is the field the dispatch chain consults before
    /// reporting a verb unknown.
    pub arity: i8,
    /// `COMMAND` flags — `readonly`, `write`, `admin`, `blocking`,
    /// `extension`. Documentation only; the write classification the AOF
    /// and replication paths actually gate on lives in
    /// `kevy_resp::ops_table`.
    pub flags: &'static [&'static str],
    /// One sentence, present tense, for the reference and `COMMAND DOCS`.
    pub summary: &'static str,
    /// The kevy version this verb first shipped in — not the Redis one.
    pub since: &'static str,
    /// The full call form, `VERB arg [optional …]`, as the reference
    /// prints it and as an arity error points a caller at.
    pub syntax: &'static str,
    /// Time complexity of THIS engine's implementation, derived by reading it.
    /// Never copied from Redis's docs: several of ours genuinely differ in
    /// both directions (LINDEX/LSET are O(1) on a VecDeque where Redis's
    /// quicklist is O(N); SSCAN copies the whole set in one batch).
    pub complexity: &'static str,
    /// How this verb differs from Redis, if it does: `full`, `differs: …`, or
    /// `kevy-only…`. This is the field a person migrating off Redis actually
    /// needs, and the one Redis's own reference cannot have.
    pub compat: &'static str,
}

#[allow(clippy::too_many_arguments)]
const fn v(
    name: &'static str,
    group: &'static str,
    arity: i8,
    flags: &'static [&'static str],
    summary: &'static str,
    since: &'static str,
    syntax: &'static str,
    complexity: &'static str,
    compat: &'static str,
) -> VerbMeta {
    VerbMeta { name, group, arity, flags, summary, since, syntax, complexity, compat }
}


/// The registry, assembled from the per-family tables in `verb_meta/`.
///
/// Split by family because one file with 183 rows would blow the 500-LOC house
/// cap, and a `const fn` concat keeps `VERB_META` a plain `&[VerbMeta]` so no
/// caller has to know it was split.
mod admin;
mod collections;
mod extensions;
mod keyspace;
mod streams_geo;

/// The flag consts the family tables build their rows from.
pub(crate) mod flags {
    pub(crate) const W: &[&str] = &["write"];
    pub(crate) const R: &[&str] = &["readonly"];
    pub(crate) const WB: &[&str] = &["write", "blocking"];
    pub(crate) const RB: &[&str] = &["readonly", "blocking"];
    /// Introspection / control — no keyspace write.
    pub(crate) const AD: &[&str] = &["readonly", "admin"];
    /// Admin verbs with durable side effects.
    pub(crate) const WAD: &[&str] = &["write", "admin"];
    pub(crate) const TX: &[&str] = &["readonly", "transaction"];
    pub(crate) const PS: &[&str] = &["readonly", "pubsub"];
    pub(crate) const RX: &[&str] = &["readonly", "extension"];
    pub(crate) const RBX: &[&str] = &["readonly", "blocking", "extension"];
    pub(crate) const WX: &[&str] = &["write", "extension"];
}

const FAMILIES: [&[VerbMeta]; 5] = [
    keyspace::ROWS,
    collections::ROWS,
    streams_geo::ROWS,
    admin::ROWS,
    extensions::ROWS,
];

const TOTAL: usize = keyspace::ROWS.len()
    + collections::ROWS.len()
    + streams_geo::ROWS.len()
    + admin::ROWS.len()
    + extensions::ROWS.len();

/// A row that exists only to seed the concat array; every slot is overwritten
/// before `VERB_META` is observable.
const BLANK: VerbMeta = v("", "", 0, flags::R, "", "", "", "", "");

const fn concat() -> [VerbMeta; TOTAL] {
    let mut out = [BLANK; TOTAL];
    let mut at = 0;
    let mut f = 0;
    while f < FAMILIES.len() {
        let fam = FAMILIES[f];
        let mut i = 0;
        while i < fam.len() {
            out[at] = fam[i];
            at += 1;
            i += 1;
        }
        f += 1;
    }
    out
}

/// A `static`, not a `const`: a const of 183 rows would be copied into every
/// use site.
static TABLE: [VerbMeta; TOTAL] = concat();

/// Every dispatch-reachable verb, in family order. The single source of truth
/// for `COMMAND DOCS`, `llms.txt`, the MCP schema, and the command reference on
/// the site — one table, so none of them can drift from the others.
pub const VERB_META: &[VerbMeta] = &TABLE;

/// Look one verb up by its uppercased name.
pub fn verb_meta(name: &str) -> Option<&'static VerbMeta> {
    VERB_META.iter().find(|m| m.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The concat must not lose or duplicate a row. A silently-truncated
    /// registry would show up as commands vanishing from COMMAND DOCS,
    /// llms.txt and the reference all at once.
    #[test]
    fn every_family_row_reaches_the_table() {
        assert_eq!(VERB_META.len(), TOTAL);
        assert_eq!(
            VERB_META.len(),
            keyspace::ROWS.len()
                + collections::ROWS.len()
                + streams_geo::ROWS.len()
                + admin::ROWS.len()
                + extensions::ROWS.len()
        );
        assert!(VERB_META.iter().all(|m| !m.name.is_empty()), "a BLANK seed row survived");
    }

    #[test]
    fn verb_names_are_unique() {
        let mut seen: Vec<&str> = VERB_META.iter().map(|m| m.name).collect();
        let before = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), before, "a verb is registered twice");
    }

    /// Both new fields carry real content on every verb. An empty complexity
    /// or an empty compat is a hole in the reference, and a hole is worse than
    /// an absent page — the reader cannot tell it from "no caveats".
    #[test]
    fn every_verb_states_its_complexity_and_its_redis_compatibility() {
        for m in VERB_META {
            assert!(!m.complexity.is_empty(), "{}: no complexity", m.name);
            assert!(!m.compat.is_empty(), "{}: no compat", m.name);
            assert!(
                m.compat == "full"
                    || m.compat.starts_with("differs:")
                    || m.compat.starts_with("kevy-only"),
                "{}: compat must be `full`, `differs: …` or `kevy-only…`, got {:?}",
                m.name,
                m.compat
            );
        }
    }

    /// The table has to tell the truth in BOTH directions: a deviation that
    /// still exists must stay documented, and one that has been fixed must not
    /// keep wearing the warning. SPOP and SRANDMEMBER used to sit in the first
    /// list — until it turned out "documented" had become a substitute for
    /// "fixed". Writing a bug down is not fixing it.
    #[test]
    fn the_costs_that_differ_from_redis_stay_differing() {
        // All three of the deviations this test used to pin (SCAN's full sweep,
        // ZRANK's O(N), SPOP's determinism) are FIXED and live in the test
        // below. What remains pinned here are the deviations that are still
        // true — a fixed one must never be re-documented, and a documented one
        // must never be quietly "corrected" toward Redis's numbers by someone
        // who did not read our code.
        let hscan = verb_meta("HSCAN").expect("HSCAN");
        assert!(hscan.compat.contains("not a cursor iterator"), "HSCAN is still single-batch");
    }

    /// The deviations we FIXED must not creep back into the table as folklore.
    /// A regression to the old behaviour would have to come here and explain
    /// itself. ZRANK's remaining WITHSCORE compat gap is a separate, still-open
    /// deviation and must not be silently dropped with the complexity fix.
    #[test]
    fn the_fixed_deviations_stay_fixed() {
        for name in ["SPOP", "SRANDMEMBER", "RANDOMKEY"] {
            let m = verb_meta(name).expect(name);
            assert_eq!(
                m.compat, "full",
                "{name} is genuinely random now; the old NOT-random note must not come back"
            );
        }
        let zrank = verb_meta("ZRANK").expect("ZRANK");
        assert!(
            zrank.complexity.contains("O(log N)"),
            "ZRANK is O(log N) now: the (score, member) tree is rank-augmented"
        );
        assert!(zrank.compat.contains("WITHSCORE"), "the WITHSCORE gap is still open");
        let zcount = verb_meta("ZCOUNT").expect("ZCOUNT");
        assert!(zcount.complexity.contains("O(log N)"), "ZCOUNT is two rank descents now");
        let scan = verb_meta("SCAN").expect("SCAN");
        assert!(
            scan.complexity.contains("O(COUNT) buckets per call"),
            "SCAN is a real cursor iterator now; the full-sweep note must not return"
        );
        assert!(scan.compat.contains("(shard, position)"), "the cursor-portability caveat stays");
    }
}

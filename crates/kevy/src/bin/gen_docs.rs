//! Generate `llms.txt` and `docs/verb-reference.md` from
//! [`kevy::verb_meta::VERB_META`], the same table COMMAND DOCS and
//! the MCP schema answer from. One source of truth, three faces.
//!
//!   gen_docs <repo-root>          — (re)write both files
//!   gen_docs <repo-root> --check  — exit 1 if either file is stale
//!                                    (the aigate phase-2 CI clamp)

use std::fmt::Write as _;
use std::path::Path;
use std::process::ExitCode;

use kevy::verb_meta::{VERB_META, VerbMeta};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| ".".into());
    let check = args.next().as_deref() == Some("--check");
    let root = Path::new(&root);

    let outputs = [
        (root.join("llms.txt"), llms_txt()),
        (root.join("docs/verb-reference.md"), verb_reference()),
    ];
    let mut stale = false;
    for (path, want) in outputs {
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        if current == want {
            continue;
        }
        if check {
            eprintln!("gen-docs: STALE {} (regenerate with `cargo run -p kevy --bin gen_docs .`)", path.display());
            stale = true;
        } else {
            std::fs::write(&path, &want).expect("write generated doc");
            println!("gen-docs: wrote {}", path.display());
        }
    }
    if stale { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

fn groups() -> Vec<(&'static str, Vec<&'static VerbMeta>)> {
    let mut out: Vec<(&'static str, Vec<&'static VerbMeta>)> = Vec::new();
    for m in VERB_META {
        match out.iter_mut().find(|(g, _)| *g == m.group) {
            Some((_, v)) => v.push(m),
            None => out.push((m.group, vec![m])),
        }
    }
    out
}

fn flags_of(m: &VerbMeta) -> String {
    m.flags.join(",")
}

fn llms_txt() -> String {
    let mut s = String::new();
    let _ = write!(
        s,
        "# kevy\n\n\
         > Pure-Rust, zero-dependency, Redis-compatible serving engine: the primary\n\
         > store for applications (declared indexes, views, write-time aggregates,\n\
         > CJK full-text search, vector KNN, CDC feeds, replication) speaking RESP.\n\n\
         Machine notes: wire protocol is RESP2 (RESP3 via HELLO 3). Every verb below\n\
         is discoverable live via `COMMAND DOCS <verb>`, which answers with the same\n\
         `complexity` and `compat` fields printed here; errors are self-explaining and\n\
         their prefixes are a stable contract (docs/error-replies.md). This file is\n\
         GENERATED from the server's verb metadata table — do not edit by hand.\n\n\
         Read `compat` before you assume a verb behaves the way Redis's docs say. kevy\n\
         is wire-compatible, not behaviour-identical, and the differences that matter\n\
         are stated per verb rather than buried in a migration guide. Three that catch\n\
         people: SCAN is not a cursor iterator (one call sweeps the whole keyspace and\n\
         returns cursor 0); RANDOMKEY, SPOP and SRANDMEMBER are NOT random (they return\n\
         the same members every time); and multi-key writes are atomic only within one\n\
         shard — co-locate keys with a {{hashtag}} when you need them to move together.\n\n\
         Read `complexity` before you assume a cost. It was derived from THIS engine's\n\
         code, not copied from Redis: our sorted set is a hash plus a plain BTreeSet\n\
         with no rank augmentation, so ZRANK is O(N) here and score-range queries scan\n\
         rather than seek.\n\n\
         ## Docs\n\n\
         - [Verb reference](docs/verb-reference.md): every verb, arity, flags, syntax\n\
         - [Designing on kevy](docs/designing-on-kevy.md): the serving-engine model\n\
         - [Cookbook](docs/cookbook.md): RDS-to-kevy modeling recipes\n\
         - [RDS workloads](docs/rds-workloads.md): SQL-to-kevy reference matrix (types, SELECT, JOIN, transactions, DDL)\n\
         - [Indexes](docs/indexes.md) · [Views](docs/views.md) · [Text search](docs/text-search.md) · [Vector search](docs/vector-search.md)\n\
         - [CDC feeds](docs/cdc.md) · [Replication](docs/replication.md) · [Availability & failover](docs/availability.md) · [Persistence](docs/persistence.md)\n\
         - [Migration](docs/migration.md) · [Upgrading between majors](docs/UPGRADING.md)\n\
         - [WASM / browser](docs/wasm.md) · [IoT / embedded tiers](docs/iot.md)\n\
         - [Error contract](docs/error-replies.md) · [Tuning](docs/tuning.md)\n\n\
         ## Verbs ({} total)\n\n",
        VERB_META.len()
    );
    for (group, verbs) in groups() {
        let _ = writeln!(s, "### {group}\n");
        for m in verbs {
            let _ = writeln!(s, "- `{}` [{}] — {}", m.syntax, flags_of(m), m.summary);
            let _ = writeln!(s, "  - complexity: {}", m.complexity);
            let _ = writeln!(s, "  - compat: {}", m.compat);
        }
        s.push('\n');
    }
    s
}

fn verb_reference() -> String {
    let mut s = String::new();
    let _ = write!(
        s,
        "# Verb reference\n\n\
         Every wire-reachable verb, from the server's own metadata table\n\
         (`COMMAND DOCS` answers from the same rows). GENERATED by\n\
         `cargo run -p kevy --bin gen_docs .` — do not edit by hand.\n\n\
         {} verbs. Flags: `write`/`readonly` (side-effect class),\n\
         `admin`, `blocking`, `pubsub`, `transaction`, `extension`\n\
         (kevy-specific surface; argument 1 is a catalog name, not a key).\n\n\
         **Complexity** is the cost of THIS engine's implementation, read out of the\n\
         code — not copied from Redis's reference. Several genuinely differ, and they\n\
         look like typos and are not: `ZRANK` is O(N) because our sorted set has no\n\
         rank-augmented structure, `ZCOUNT` and `ZRANGEBYSCORE` scan rather than seek,\n\
         and every geo search is O(N).\n\n\
         **Redis compatibility** is `full`, `differs: …`, or `kevy-only`. This is the\n\
         column to read before a migration; it is also the column Redis's own reference\n\
         cannot have.\n\n",
        VERB_META.len()
    );
    for (group, verbs) in groups() {
        let _ = writeln!(s, "## {group}\n");
        let _ = writeln!(s, "| Verb | Arity | Flags | Complexity | Redis | Summary |");
        let _ = writeln!(s, "|---|---|---|---|---|---|");
        for m in verbs {
            let _ = writeln!(
                s,
                "| `{}` | {} | {} | {} | {} | {} |",
                m.syntax.replace('|', "\\|"),
                m.arity,
                flags_of(m),
                m.complexity.replace('|', "\\|"),
                m.compat.replace('|', "\\|"),
                m.summary.replace('|', "\\|")
            );
        }
        s.push('\n');
    }
    s
}

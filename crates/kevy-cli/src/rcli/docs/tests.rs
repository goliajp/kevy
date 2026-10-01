//! The reference built from hand-made `COMMAND DOCS` replies: what it holds,
//! and what help, hints and completion make of it.
//!
//! The expected hints follow the matching rules in `marks.rs`; that those
//! rules are redis-cli's is bench/cligate.py's question, asked of the real
//! redis-cli 8.10.1 against a real server's docs.

use super::help_text;
use super::model::{Docs, Params};
use kevy_resp::Reply;

fn s(t: &str) -> Reply {
    Reply::Bulk(t.as_bytes().to_vec())
}

fn map(pairs: Vec<(&str, Reply)>) -> Reply {
    Reply::Array(pairs.into_iter().flat_map(|(k, v)| [s(k), v]).collect())
}

/// An argument spec: name, type, then optional token and flags.
fn arg(name: &str, kind: &str, token: Option<&str>, flags: &[&str], children: Vec<Reply>) -> Reply {
    let mut fields = vec![("name", s(name)), ("type", s(kind))];
    if let Some(t) = token {
        fields.push(("token", s(t)));
    }
    if !flags.is_empty() {
        fields.push((
            "flags",
            Reply::Array(flags.iter().map(|f| Reply::Simple(f.as_bytes().to_vec())).collect()),
        ));
    }
    if !children.is_empty() {
        fields.push(("arguments", Reply::Array(children)));
    }
    map(fields)
}

fn command(summary: &str, group: &str, args: Vec<Reply>) -> Reply {
    map(vec![
        ("summary", s(summary)),
        ("since", s("1.0.0")),
        ("group", s(group)),
        ("arguments", Reply::Array(args)),
    ])
}

fn set_like() -> Reply {
    command(
        "Set a value.",
        "string",
        vec![
            arg("key", "key", None, &[], vec![]),
            arg("value", "string", None, &[], vec![]),
            arg(
                "condition",
                "oneof",
                None,
                &["optional"],
                vec![
                    arg("nx", "pure-token", Some("NX"), &[], vec![]),
                    arg("xx", "pure-token", Some("XX"), &[], vec![]),
                ],
            ),
            arg("get", "pure-token", Some("GET"), &["optional"], vec![]),
            arg(
                "expiration",
                "oneof",
                None,
                &["optional"],
                vec![
                    arg("seconds", "integer", Some("EX"), &[], vec![]),
                    arg("unix-time-milliseconds", "unix-time", Some("PXAT"), &[], vec![]),
                    arg("keepttl", "pure-token", Some("KEEPTTL"), &[], vec![]),
                ],
            ),
        ],
    )
}

fn zadd_like() -> Reply {
    command(
        "Add members.",
        "sorted-set",
        vec![
            arg("key", "key", None, &[], vec![]),
            arg("change", "pure-token", Some("CH"), &["optional"], vec![]),
            arg(
                "data",
                "block",
                None,
                &["multiple"],
                vec![
                    arg("score", "double", None, &[], vec![]),
                    arg("member", "string", None, &[], vec![]),
                ],
            ),
        ],
    )
}

fn many_tokens() -> Reply {
    command(
        "Repeat tokens.",
        "generic",
        vec![
            arg("weight", "integer", Some("WEIGHTS"), &["optional", "multiple"], vec![]),
            arg("pair", "string", Some("BY"), &["optional", "multiple", "multiple_token"], vec![]),
        ],
    )
}

fn client_like() -> Reply {
    let kill = command(
        "Kill a connection.",
        "connection",
        vec![arg("id", "integer", Some("ID"), &[], vec![])],
    );
    map(vec![
        ("summary", s("Client commands.")),
        ("group", s("connection")),
        (
            "subcommands",
            map(vec![
                ("client|kill", kill),
                ("client|list", command("List.", "connection", vec![])),
            ]),
        ),
    ])
}

fn docs() -> Docs {
    let table = map(vec![
        ("zadd", zadd_like()),
        ("set", set_like()),
        ("repeat", many_tokens()),
        ("client", client_like()),
    ]);
    Docs::from_reply(&table).expect("a docs table")
}

fn hint(line: &str) -> Option<String> {
    docs().hint(line.as_bytes()).map(|h| String::from_utf8_lossy(&h).into_owned())
}

#[test]
fn entries_are_named_by_upper_case_words_in_name_order() {
    let d = docs();
    let names: Vec<String> =
        d.entries.iter().map(|e| String::from_utf8_lossy(&e.full).into_owned()).collect();
    assert_eq!(names, ["CLIENT", "CLIENT KILL", "CLIENT LIST", "REPEAT", "SET", "ZADD"]);
    let groups: Vec<&[u8]> = d.groups.iter().map(Vec::as_slice).collect();
    assert_eq!(groups, [&b"connection"[..], b"generic", b"sorted-set", b"string"]);
}

#[test]
fn a_reply_that_is_not_a_table_builds_nothing() {
    assert_eq!(Docs::from_reply(&Reply::Error(b"ERR unknown".to_vec())), None);
    assert_eq!(Docs::from_reply(&Reply::Array(vec![s("odd")])), None);
}

#[test]
fn a_syntax_line_loses_the_words_that_name_the_command() {
    let spec =
        map(vec![("summary", s("Configure.")), ("syntax", s("CONFIG GET parameter | REWRITE"))]);
    let d = Docs::from_reply(&map(vec![("config", spec)])).expect("a docs table");
    assert_eq!(d.entries[0].params, Params::Syntax(b"GET parameter | REWRITE".to_vec()));
    assert_eq!(d.hint(b"config ").as_deref(), Some(&b"GET parameter | REWRITE"[..]));
    assert_eq!(
        d.hint(b"config get ").as_deref(),
        Some(&b""[..]),
        "a syntax line cannot follow typed words"
    );
}

#[test]
fn the_hint_is_what_the_typed_words_leave_open() {
    let full = "key value [NX|XX] [GET] [EX seconds|PXAT unix-time-milliseconds|KEEPTTL]";
    assert_eq!(hint("set ").as_deref(), Some(full));
    assert_eq!(
        hint("SET k").as_deref(),
        Some(full),
        "the last word counts once a blank follows it"
    );
    assert_eq!(
        hint("set k v ").as_deref(),
        Some("[NX|XX] [GET] [EX seconds|PXAT unix-time-milliseconds|KEEPTTL]")
    );
    assert_eq!(
        hint("set k v get nx ").as_deref(),
        Some("[EX seconds|PXAT unix-time-milliseconds|KEEPTTL]")
    );
    assert_eq!(
        hint("set k v ex ").as_deref(),
        Some("seconds [NX|XX] [GET] "),
        "the one being typed comes first"
    );
    assert_eq!(hint("set k v ex 10 ").as_deref(), Some("[NX|XX] [GET] "));
}

#[test]
fn a_word_of_the_wrong_type_leaves_no_hint() {
    assert_eq!(hint("set k v ex soon ").as_deref(), Some(""));
    assert_eq!(
        hint("set k v pxat 12abc ").as_deref(),
        Some("[NX|XX] [GET] "),
        "only the leading digits count"
    );
    assert_eq!(hint("set k v bogus ").as_deref(), Some(""));
}

#[test]
fn a_line_naming_no_command_has_no_hint() {
    assert_eq!(hint("nosuch "), None);
    assert_eq!(hint("set"), None, "the command word itself is not typed until a blank follows");
    assert_eq!(hint("set \"k "), None, "an unclosed quote is not a line");
    assert_eq!(hint(""), None);
}

#[test]
fn a_subcommand_is_found_before_its_container() {
    assert_eq!(hint("client kill ").as_deref(), Some("ID id"));
    assert_eq!(hint("client ").as_deref(), Some(""));
}

#[test]
fn a_repeating_block_shows_its_unfinished_part_then_the_repetition() {
    assert_eq!(hint("zadd k ").as_deref(), Some("[CH] score member [score member ...]"));
    assert_eq!(hint("zadd k 1 ").as_deref(), Some("member [score member ...]"));
    assert_eq!(hint("zadd k ch 1 m ").as_deref(), Some("[score member ...]"));
    assert_eq!(hint("zadd k .5 m inf n ").as_deref(), Some("[score member ...]"));
    assert_eq!(hint("zadd k x ").as_deref(), Some(""), "a score must start like a number");
}

#[test]
fn a_repeated_token_is_shown_once_or_every_time() {
    assert_eq!(
        hint("repeat ").as_deref(),
        Some("[WEIGHTS weight [weight ...]] [BY pair [BY pair ...]]")
    );
    assert_eq!(
        hint("repeat weights 1 2 ").as_deref(),
        Some("[weight ...] [BY pair [BY pair ...]]")
    );
    assert_eq!(
        hint("repeat by a by b ").as_deref(),
        Some("[BY pair ...] [WEIGHTS weight [weight ...]] ")
    );
}

#[test]
fn brackets_are_closed_only_where_they_were_opened() {
    // DEV-014: redis-cli closes one more bracket than it opens here.
    let h = hint("repeat weights 1 2 ").unwrap_or_default();
    assert_eq!(h.matches('[').count(), h.matches(']').count(), "{h}");
}

#[test]
fn tab_offers_command_names_and_after_help_the_groups() {
    let d = docs();
    let offered = |line: &str| -> Vec<String> {
        d.completions(line.as_bytes())
            .iter()
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect()
    };
    assert_eq!(offered("cl"), ["CLIENT", "CLIENT KILL", "CLIENT LIST"]);
    assert_eq!(offered("client k"), ["CLIENT KILL"]);
    assert_eq!(offered("@"), Vec::<String>::new(), "groups only after help");
    assert_eq!(offered("help  @s"), ["help  @sorted-set", "help  @string"]);
    assert_eq!(offered("HELP s"), ["HELP SET"]);
    assert_eq!(offered("set k"), Vec::<String>::new());
}

#[test]
fn help_for_a_command_prints_its_block_with_the_group() {
    let text = help_text::topic(&docs(), &[b"client".to_vec(), b"kill".to_vec()]);
    let want = "\r\n  \x1b[1mCLIENT KILL\x1b[0m \x1b[90mID id\x1b[0m\r\n  \x1b[33msummary:\x1b[0m Kill a connection.\r\n  \x1b[33msince:\x1b[0m 1.0.0\r\n  \x1b[33mgroup:\x1b[0m connection\r\n\r\n";
    assert_eq!(String::from_utf8_lossy(&text), want);
}

#[test]
fn help_for_a_group_lists_its_commands_without_the_group_line() {
    let text = String::from_utf8_lossy(&help_text::topic(&docs(), &[b"@CONNECTION".to_vec()]))
        .into_owned();
    assert_eq!(text.matches("\x1b[1m").count(), 3, "{text}");
    assert!(!text.contains("group:"), "{text}");
    // DEV-012: redis-cli prints `(null)` where a command has no argument docs.
    assert!(text.contains("\x1b[1mCLIENT\x1b[0m \x1b[90m\x1b[0m\r\n  \x1b[33msummary:\x1b[0m Client commands.\r\n\r\n"), "{text}");
}

#[test]
fn help_for_nothing_known_is_a_line_break() {
    assert_eq!(help_text::topic(&docs(), &[b"nosuch".to_vec()]), b"\r\n");
    assert_eq!(
        help_text::topic(&docs(), &[b"client".to_vec(), b"kill".to_vec(), b"x".to_vec()]),
        b"\r\n"
    );
}

#[test]
fn kevys_compat_note_is_shown_only_when_it_says_something() {
    let spec =
        |compat: &str| map(vec![("summary", s("x")), ("group", s("g")), ("compat", s(compat))]);
    let d = Docs::from_reply(&map(vec![("a", spec("full")), ("b", spec("differs: no LIMIT"))]))
        .expect("table");
    let a = String::from_utf8_lossy(&help_text::topic(&d, &[b"a".to_vec()])).into_owned();
    let b = String::from_utf8_lossy(&help_text::topic(&d, &[b"b".to_vec()])).into_owned();
    assert!(!a.contains("compat"), "{a}");
    assert!(b.contains("\x1b[33mcompat:\x1b[0m differs: no LIMIT\r\n"), "{b}");
}

#[test]
fn offline_docs_are_kevys_own_reference() {
    let d = Docs::offline();
    assert!(d.entries.len() > 150, "{} entries", d.entries.len());
    let get = d.lookup(&[b"get".to_vec()]).expect("GET is documented");
    assert_eq!(get.params, Params::Syntax(b"key".to_vec()));
    assert!(d.groups.iter().any(|g| g == b"string"));
}

#[test]
fn the_overview_names_kevy_cli_and_its_preferences_file() {
    let text = String::from_utf8_lossy(&help_text::overview()).into_owned();
    assert!(text.starts_with(concat!("kevy-cli ", env!("CARGO_PKG_VERSION"), "\n")), "{text}");
    assert!(text.ends_with("Set your preferences in ~/.kevyclirc\n"), "{text}");
}

/// RESP3 servers answer COMMAND DOCS with maps, sets and verbatim strings.
#[test]
fn a_resp3_shaped_table_reads_the_same() {
    let flags = Reply::Set(vec![Reply::Simple(b"optional".to_vec())]);
    let arg = Reply::Map(vec![
        (s("name"), s("n")),
        (s("display_text"), Reply::Verbatim { fmt: *b"txt", data: b"count".to_vec() }),
        (s("type"), s("integer")),
        (s("flags"), flags),
    ]);
    let spec = Reply::Map(vec![
        (s("summary"), Reply::Verbatim { fmt: *b"txt", data: b"Pop.".to_vec() }),
        (s("group"), s("list")),
        (s("arguments"), Reply::Array(vec![arg])),
    ]);
    let d = Docs::from_reply(&Reply::Map(vec![(s("lpop"), spec)])).expect("a RESP3 table");
    assert_eq!(d.entries[0].summary.as_deref(), Some(&b"Pop."[..]));
    assert_eq!(d.hint(b"lpop ").as_deref(), Some(&b"[count]"[..]), "display_text names the value");
}

#[test]
fn a_malformed_table_is_refused_whole() {
    let bad = [
        map(vec![]).clone(),
        Reply::Array(vec![Reply::Int(1), map(vec![("summary", s("x"))])]),
        Reply::Array(vec![s("a"), Reply::Int(1)]),
        Reply::Array(vec![s("a"), Reply::Array(vec![Reply::Int(1), s("x")])]),
        map(vec![("a", map(vec![("arguments", s("not a list"))]))]),
        map(vec![("a", map(vec![("arguments", Reply::Array(vec![Reply::Int(3)]))]))]),
        map(vec![(
            "a",
            map(vec![("arguments", Reply::Array(vec![map(vec![("flags", s("x"))])]))]),
        )]),
        map(vec![(
            "a",
            map(vec![(
                "arguments",
                Reply::Array(vec![map(vec![("flags", Reply::Array(vec![Reply::Int(1)]))])]),
            )]),
        )]),
        map(vec![(
            "a",
            map(vec![("arguments", Reply::Array(vec![map(vec![("arguments", s("x"))])]))]),
        )]),
        map(vec![("a", map(vec![("subcommands", s("x"))]))]),
        map(vec![(
            "a",
            map(vec![("subcommands", Reply::Array(vec![Reply::Int(1), map(vec![])]))]),
        )]),
        map(vec![("a", map(vec![("subcommands", map(vec![("a|b", s("x"))]))]))]),
    ];
    // The first is an empty table: valid, and empty.
    assert_eq!(Docs::from_reply(&bad[0]).map(|d| d.entries.len()), Some(0));
    for (i, reply) in bad.iter().enumerate().skip(1) {
        assert_eq!(Docs::from_reply(reply), None, "case {i}");
    }
}

#[test]
fn unknown_fields_and_flags_are_ignored() {
    let arg = map(vec![
        ("name", s("k")),
        ("type", s("key")),
        ("key_spec_index", Reply::Int(0)),
        ("flags", Reply::Array(vec![Reply::Simple(b"deprecated".to_vec())])),
    ]);
    let spec = map(vec![
        ("summary", s("x")),
        ("doc_flags", Reply::Array(vec![])),
        ("arguments", Reply::Array(vec![arg])),
    ]);
    let d = Docs::from_reply(&map(vec![("get", spec)])).expect("a table");
    assert_eq!(d.hint(b"get ").as_deref(), Some(&b"k"[..]));
}

#[test]
fn subcommands_without_a_container_prefix_keep_their_name() {
    let spec = map(vec![
        ("summary", s("x")),
        ("subcommands", map(vec![("list", command("L.", "g", vec![]))])),
    ]);
    let d = Docs::from_reply(&map(vec![("thing", spec)])).expect("a table");
    assert!(d.entries.iter().any(|e| e.full == b"THING LIST"));
}

#[test]
fn a_syntax_line_that_does_not_start_with_the_name_is_kept_whole() {
    let spec = map(vec![("syntax", s("kevy extension: pops things"))]);
    let d = Docs::from_reply(&map(vec![("zpop.below", spec)])).expect("a table");
    assert_eq!(d.entries[0].params, Params::Syntax(b"kevy extension: pops things".to_vec()));
}

#[test]
fn every_repeating_shape_renders() {
    let shapes = command(
        "x",
        "g",
        vec![
            arg(
                "flag",
                "pure-token",
                Some("F"),
                &["optional", "multiple", "multiple_token"],
                vec![],
            ),
            arg(
                "choice",
                "oneof",
                None,
                &["optional", "multiple"],
                vec![
                    arg("a", "pure-token", Some("A"), &[], vec![]),
                    arg("b", "string", Some("B"), &[], vec![]),
                ],
            ),
            arg("", "string", None, &["optional"], vec![]),
            arg("unnamed", "pure-token", None, &["optional"], vec![]),
        ],
    );
    let d = Docs::from_reply(&map(vec![("x", shapes)])).expect("a table");
    let full = String::from_utf8_lossy(&d.hint(b"x ").unwrap_or_default()).into_owned();
    // A pure token with no token of its own has nothing to show.
    assert_eq!(full, r#"[F [F ...]] [A|B b [A|B b ...]] [""] []"#);
}

#[test]
fn doubles_by_name_and_the_longest_command_wins() {
    let d = docs();
    assert_eq!(hint("zadd k INF m NaN n -inf o ").as_deref(), Some("[score member ...]"));
    assert_eq!(hint("zadd k in ").as_deref(), Some(""), "`in` is not a number");
    // CLIENT and CLIENT KILL both prefix `client kill x`; the longer names it.
    assert_eq!(
        d.lookup(&[b"CLIENT".to_vec(), b"KILL".to_vec(), b"x".to_vec()]).map(|e| e.full.clone()),
        Some(b"CLIENT KILL".to_vec())
    );
}

//! Verb catalog bootstrapped from the live server's `COMMAND DOCS` reply
//! (the server's single-source-of-truth verb table). kevy-mcp hardcodes no
//! verb list — whatever server it connects to defines the tool surface,
//! so the read/write whitelists can never drift from the engine.
//!
//! Wire shape (see `crates/kevy/src/cmd_command.rs`): a flat array of
//! `[name, fieldmap]` pairs, where `fieldmap` is a 10-element array of
//! alternating keys and values: `summary`, `since`, `group`, `syntax`
//! (bulk strings) and `flags` (array of bulk strings).

use std::collections::HashSet;

use kevy_resp_client::Reply;

use crate::json::{Value, obj, s};

/// One verb row lifted out of the COMMAND DOCS pairs.
#[derive(Debug)]
pub struct VerbDoc {
    /// Verb name as reported by the server (upper-case).
    pub name: String,
    /// One-line human summary.
    pub summary: String,
    /// Version the verb appeared in.
    pub since: String,
    /// Family group (`string`, `hash`, `extension`, …).
    pub group: String,
    /// Full call syntax, e.g. `SET key value [EX seconds]`.
    pub syntax: String,
    /// Behavior flags; `write` drives the whitelist split.
    pub flags: Vec<String>,
}

/// Read/write classification of a verb, from the DOCS `flags` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// No `write` flag — allowed through `kevy_read`.
    Read,
    /// Has the `write` flag — gated behind `--allow-writes`.
    Write,
}

/// The bootstrapped whitelist: every server verb, split by [`Class`].
pub struct Catalog {
    read: HashSet<String>,
    write: HashSet<String>,
}

impl Catalog {
    /// Build the catalog from a full `COMMAND DOCS` reply.
    pub fn from_docs_reply(reply: &Reply) -> Result<Self, String> {
        let docs = parse_docs(reply)?;
        if docs.is_empty() {
            return Err("COMMAND DOCS returned no verbs — server too old?".into());
        }
        let mut read = HashSet::new();
        let mut write = HashSet::new();
        for d in &docs {
            let name = d.name.to_ascii_uppercase();
            // blocking verbs would stall kevy-mcp's single connection
            // and pubsub verbs would wedge it into subscriber mode —
            // neither is meaningful through a request/response tool,
            // so they are excluded from BOTH whitelists.
            if d.flags.iter().any(|f| f == "blocking" || f == "pubsub") {
                continue;
            }
            if d.flags.iter().any(|f| f == "write") {
                write.insert(name);
            } else {
                read.insert(name);
            }
        }
        Ok(Self { read, write })
    }

    /// Classify an upper-cased verb; `None` when the server never
    /// reported it.
    pub fn classify(&self, verb_upper: &str) -> Option<Class> {
        if self.read.contains(verb_upper) {
            Some(Class::Read)
        } else if self.write.contains(verb_upper) {
            Some(Class::Write)
        } else {
            None
        }
    }

    /// Number of readonly verbs (startup banner).
    pub fn read_count(&self) -> usize {
        self.read.len()
    }

    /// Number of write verbs (startup banner).
    pub fn write_count(&self) -> usize {
        self.write.len()
    }
}

/// Parse a `COMMAND DOCS` reply into verb rows.
pub fn parse_docs(reply: &Reply) -> Result<Vec<VerbDoc>, String> {
    let items = match reply {
        Reply::Array(items) => items,
        Reply::Error(e) => {
            return Err(format!(
                "server rejected COMMAND DOCS: {}",
                String::from_utf8_lossy(e)
            ));
        }
        other => return Err(format!("COMMAND DOCS: expected array reply, got {other:?}")),
    };
    if items.len() % 2 != 0 {
        return Err(format!(
            "COMMAND DOCS: expected flat [name, fields] pairs, got {} items",
            items.len()
        ));
    }
    let mut docs = Vec::with_capacity(items.len() / 2);
    for pair in items.chunks_exact(2) {
        let name = text(&pair[0])
            .ok_or_else(|| "COMMAND DOCS: verb name is not a string".to_string())?;
        docs.push(parse_fields(name, &pair[1])?);
    }
    Ok(docs)
}

/// JSON-ify a `COMMAND DOCS` reply as
/// `{"VERB": {"summary": …, "since": …, "group": …, "syntax": …, "flags": […]}, …}`.
pub fn docs_json(reply: &Reply) -> Result<Value, String> {
    let docs = parse_docs(reply)?;
    Ok(Value::Object(
        docs.into_iter()
            .map(|d| {
                let fields = obj(vec![
                    ("summary", s(&d.summary)),
                    ("since", s(&d.since)),
                    ("group", s(&d.group)),
                    ("syntax", s(&d.syntax)),
                    ("flags", Value::Array(d.flags.iter().map(|f| s(f)).collect())),
                ]);
                (d.name, fields)
            })
            .collect(),
    ))
}

fn parse_fields(name: String, fields: &Reply) -> Result<VerbDoc, String> {
    let Reply::Array(kv) = fields else {
        return Err(format!("COMMAND DOCS '{name}': fields are not an array"));
    };
    let mut doc = VerbDoc {
        name,
        summary: String::new(),
        since: String::new(),
        group: String::new(),
        syntax: String::new(),
        flags: Vec::new(),
    };
    for f in kv.chunks_exact(2) {
        let key = text(&f[0]).ok_or_else(|| {
            format!("COMMAND DOCS '{}': field key is not a string", doc.name)
        })?;
        match key.as_str() {
            "summary" => doc.summary = field_text(&doc.name, &key, &f[1])?,
            "since" => doc.since = field_text(&doc.name, &key, &f[1])?,
            "group" => doc.group = field_text(&doc.name, &key, &f[1])?,
            "syntax" => doc.syntax = field_text(&doc.name, &key, &f[1])?,
            "flags" => doc.flags = flag_list(&doc.name, &f[1])?,
            _ => {} // unknown keys: field maps are extensible by contract
        }
    }
    Ok(doc)
}

fn field_text(verb: &str, key: &str, v: &Reply) -> Result<String, String> {
    text(v).ok_or_else(|| format!("COMMAND DOCS '{verb}': field '{key}' is not a string"))
}

fn flag_list(verb: &str, v: &Reply) -> Result<Vec<String>, String> {
    let Reply::Array(items) = v else {
        return Err(format!("COMMAND DOCS '{verb}': flags are not an array"));
    };
    items
        .iter()
        .map(|f| {
            text(f).ok_or_else(|| format!("COMMAND DOCS '{verb}': flag is not a string"))
        })
        .collect()
}

fn text(r: &Reply) -> Option<String> {
    match r {
        Reply::Bulk(b) | Reply::Simple(b) => Some(String::from_utf8_lossy(b).into_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded from a live kevy server answering
    /// `COMMAND DOCS GET SET IDX.CREATE` — raw wire bytes, unedited.
    const DOCS_SAMPLE: &[u8] = b"*6\r\n$3\r\nGET\r\n*10\r\n$7\r\nsummary\r\n$33\r\nReturn the string \
        value of a key.\r\n$5\r\nsince\r\n$5\r\n1.0.0\r\n$5\r\ngroup\r\n$6\r\
        \nstring\r\n$6\r\nsyntax\r\n$7\r\nGET key\r\n$5\r\nflags\r\n*1\r\n$8\
        \r\nreadonly\r\n$3\r\nSET\r\n*10\r\n$7\r\nsummary\r\n$68\r\nSet a ke\
        y's string value with optional TTL and existence conditions.\r\n$5\r\
        \nsince\r\n$5\r\n1.0.0\r\n$5\r\ngroup\r\n$6\r\nstring\r\n$6\r\nsynta\
        x\r\n$50\r\nSET key value [EX seconds|PX milliseconds] [NX|XX]\r\n$5\
        \r\nflags\r\n*1\r\n$5\r\nwrite\r\n$10\r\nIDX.CREATE\r\n*10\r\n$7\r\n\
        summary\r\n$82\r\nDeclare a secondary index over a key prefix (catal\
        og mutation, sidecar-persisted).\r\n$5\r\nsince\r\n$5\r\n3.0.0\r\n$5\
        \r\ngroup\r\n$5\r\nindex\r\n$6\r\nsyntax\r\n$178\r\nIDX.CREATE name \
        ON PREFIX prefix FIELD field TYPE i64|f64|str|vector KIND range|uniq\
        ue|text|ann|agg [MAXMEM bytes] [DIM dim] [DISTANCE cosine|l2|ip] [M \
        m] [EF ef] [GROUPBY field]\r\n$5\r\nflags\r\n*2\r\n$5\r\nwrite\r\n$9\
        \r\nextension\r\n";

    fn sample_reply() -> Reply {
        let (reply, used) = kevy_resp::parse_reply(DOCS_SAMPLE)
            .expect("sample is well-formed")
            .expect("sample is complete");
        assert_eq!(used, DOCS_SAMPLE.len(), "sample fully consumed");
        reply
    }

    #[test]
    fn recorded_docs_parse_into_verb_rows() {
        let docs = parse_docs(&sample_reply()).expect("parses");
        assert_eq!(docs.len(), 3);
        let get = &docs[0];
        assert_eq!(get.name, "GET");
        assert_eq!(get.summary, "Return the string value of a key.");
        assert_eq!(get.since, "1.0.0");
        assert_eq!(get.group, "string");
        assert_eq!(get.syntax, "GET key");
        assert_eq!(get.flags, ["readonly"]);
        let idx = &docs[2];
        assert_eq!(idx.name, "IDX.CREATE");
        assert_eq!(idx.group, "index");
        assert_eq!(idx.flags, ["write", "extension"]);
    }

    #[test]
    fn whitelist_split_follows_the_write_flag() {
        let cat = Catalog::from_docs_reply(&sample_reply()).expect("builds");
        assert_eq!(cat.classify("GET"), Some(Class::Read));
        assert_eq!(cat.classify("SET"), Some(Class::Write));
        assert_eq!(cat.classify("IDX.CREATE"), Some(Class::Write));
        assert_eq!(cat.classify("NOPE"), None);
        assert_eq!(cat.read_count(), 1);
        assert_eq!(cat.write_count(), 2);
    }

    #[test]
    fn docs_json_is_keyed_by_verb() {
        let table = docs_json(&sample_reply()).expect("json-ifies");
        let set = table.get("SET").expect("SET row present");
        assert_eq!(
            set.get("syntax").and_then(Value::as_str),
            Some("SET key value [EX seconds|PX milliseconds] [NX|XX]")
        );
        let flags = set.get("flags").and_then(Value::as_array).expect("flags");
        assert_eq!(flags, [s("write")]);
    }

    #[test]
    fn error_and_odd_shapes_rejected() {
        let err = parse_docs(&Reply::Error(b"ERR nope".to_vec())).unwrap_err();
        assert!(err.contains("ERR nope"));
        let odd = Reply::Array(vec![Reply::Bulk(b"GET".to_vec())]);
        assert!(parse_docs(&odd).is_err());
        assert!(Catalog::from_docs_reply(&Reply::Array(Vec::new())).is_err());
    }
}

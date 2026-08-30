//! MCP tool surface: definitions (`tools/list`) and execution
//! (`tools/call`). Five tools — discover / read / write / explain /
//! info — all backed by one RESP connection and the [`Catalog`]
//! whitelist bootstrapped from `COMMAND DOCS`.

use kevy_resp_client::{Reply, RespClient};

use crate::catalog::{Catalog, Class, docs_json};
use crate::json::{Value, obj, s};

/// JSON-RPC error: `(code, message)`.
pub type RpcError = (i64, String);

/// JSON-RPC `-32602` Invalid params.
pub const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC `-32603` Internal error.
pub const INTERNAL: i64 = -32603;

/// Everything a tool call needs: the live connection, the verb catalog
/// bootstrapped from it, and the write opt-in flag.
pub struct ToolCx<'a> {
    /// RESP connection to the kevy server.
    pub client: &'a mut RespClient,
    /// Verb whitelist from `COMMAND DOCS`.
    pub catalog: &'a Catalog,
    /// `--allow-writes` was passed.
    pub allow_writes: bool,
}

/// What `kevy_discover` tells the agent it is for.
const DISCOVER_DESC: &str = "Discover kevy's command surface. Returns the server's live verb \
     documentation table (summary, since, group, syntax, flags) as a JSON object keyed by verb \
     name, straight from COMMAND DOCS. Pass 'verb' to fetch a single verb. Call this first when \
     unsure which command exists or what its exact syntax is.";
/// What `kevy_read` tells the agent it is for.
const READ_DESC: &str = "Execute a read-only kevy command. 'command' is the argv array, e.g. \
     [\"GET\", \"user:1\"] or [\"IDX.QUERY\", \"myidx\", \"MATCH\", \"hello\"]. Only verbs the \
     server flags as read-only are allowed; write verbs are rejected (use kevy_write). The RESP \
     reply is returned as JSON.";
/// What `kevy_write` tells the agent it is for.
const WRITE_DESC: &str = "Execute a write kevy command (SET, DEL, LPUSH, IDX.CREATE, …). \
     'command' is the argv array, e.g. [\"SET\", \"user:1\", \"alice\"]. Only verbs the server \
     flags as writes are allowed; read-only verbs belong in kevy_read. The RESP reply is \
     returned as JSON.";
/// What `kevy_explain` tells the agent it is for.
const EXPLAIN_DESC: &str = "Explain how kevy would execute an index query: passes through to \
     IDX.EXPLAIN <index> [args…] and returns the structured plan (index selection, estimated \
     rows, combination tree) as JSON.";
/// What `kevy_info` tells the agent it is for.
const INFO_DESC: &str = "Fetch kevy server statistics via INFO [section]. Returns the raw INFO \
     text (sections like server, clients, memory, stats, replication, keyspace).";

/// One tool descriptor: `{name, description, inputSchema}`.
fn tool(name: &str, desc: &str, input_schema: Value) -> Value {
    obj(vec![("name", s(name)), ("description", s(desc)), ("inputSchema", input_schema)])
}

/// Hand-built JSON Schema: `{type:"object", properties, required?}`.
fn schema(props: Vec<(&str, Value)>, required: &[&str]) -> Value {
    let mut fields = vec![("type", s("object")), ("properties", obj(props))];
    if !required.is_empty() {
        fields.push(("required", Value::Array(required.iter().map(|r| s(r)).collect())));
    }
    obj(fields)
}

/// A schema property of type `string`.
fn string_prop(desc: &str) -> Value {
    obj(vec![("type", s("string")), ("description", s(desc))])
}

/// A schema property of type `array` whose items are strings.
fn string_array_prop(desc: &str) -> Value {
    obj(vec![
        ("type", s("array")),
        ("items", obj(vec![("type", s("string"))])),
        ("description", s(desc)),
    ])
}

/// Build the `tools/list` result. `kevy_write` is only advertised when
/// the server was started with `--allow-writes`.
pub fn tools_list(allow_writes: bool) -> Value {
    let mut tools = vec![discover_tool(), read_tool()];
    if allow_writes {
        tools.push(write_tool());
    }
    tools.push(explain_tool());
    tools.push(info_tool());
    obj(vec![("tools", Value::Array(tools))])
}

/// The `kevy_discover` descriptor. `verb` is optional — omitting it
/// returns the whole table.
fn discover_tool() -> Value {
    let verb = string_prop(
        "Optional verb name to fetch docs for (e.g. \"SET\"); omit for the full table.",
    );
    tool("kevy_discover", DISCOVER_DESC, schema(vec![("verb", verb)], &[]))
}

/// The `kevy_read` descriptor.
fn read_tool() -> Value {
    let command = string_array_prop("The command as an argv array of strings, verb first.");
    tool("kevy_read", READ_DESC, schema(vec![("command", command)], &["command"]))
}

/// The `kevy_write` descriptor. Built only when writes are allowed, so an
/// agent that must not write never sees that the tool exists.
fn write_tool() -> Value {
    let command = string_array_prop("The command as an argv array of strings, verb first.");
    tool("kevy_write", WRITE_DESC, schema(vec![("command", command)], &["command"]))
}

/// The `kevy_explain` descriptor.
fn explain_tool() -> Value {
    let index = string_prop("Name of the index to explain against.");
    let args =
        string_array_prop("Query arguments passed through to IDX.EXPLAIN after the index name.");
    tool("kevy_explain", EXPLAIN_DESC, schema(vec![("index", index), ("args", args)], &["index"]))
}

/// The `kevy_info` descriptor.
fn info_tool() -> Value {
    let section = string_prop(
        "Optional INFO section (server, clients, memory, stats, replication, keyspace).",
    );
    tool("kevy_info", INFO_DESC, schema(vec![("section", section)], &[]))
}

/// Execute one `tools/call`. `Ok` is a tool result (which may itself
/// carry `isError:true` for server-side `-ERR` replies); `Err` is a
/// JSON-RPC protocol error (unknown tool, bad arguments, gated write).
pub fn call_tool(cx: &mut ToolCx, name: &str, args: &Value) -> Result<Value, RpcError> {
    match name {
        "kevy_discover" => discover(cx, args),
        "kevy_read" => run_command(cx, args, Class::Read),
        "kevy_write" => {
            if !cx.allow_writes {
                return Err((
                    INVALID_PARAMS,
                    "writes disabled (start kevy-mcp with --allow-writes)".to_string(),
                ));
            }
            run_command(cx, args, Class::Write)
        }
        "kevy_explain" => explain(cx, args),
        "kevy_info" => info(cx, args),
        other => Err((INVALID_PARAMS, format!("unknown tool '{other}'"))),
    }
}

/// Runs `COMMAND DOCS`, optionally for one verb, as a JSON table.
///
/// A server-side error comes back as a tool result with `isError`, not as
/// a JSON-RPC error: the call itself succeeded, and the agent should read
/// what the server said rather than see the transport fail.
fn discover(cx: &mut ToolCx, args: &Value) -> Result<Value, RpcError> {
    let mut argv: Vec<Vec<u8>> = vec![b"COMMAND".to_vec(), b"DOCS".to_vec()];
    if let Some(verb) = args.get("verb").and_then(Value::as_str) {
        argv.push(verb.as_bytes().to_vec());
    }
    let reply = request(cx, &argv)?;
    if let Reply::Error(e) = &reply {
        return Ok(text_result(String::from_utf8_lossy(e).into_owned(), true));
    }
    let table = docs_json(&reply).map_err(|e| (INTERNAL, e))?;
    Ok(text_result(table.serialize(), false))
}

/// The shared body of `kevy_read` and `kevy_write`.
///
/// `want` is the class the calling tool is permitted to run, and the
/// mismatch cases are separate messages on purpose: an agent told
/// "'SET' is a write verb — use kevy_write" can correct itself, where a
/// bare refusal leaves it guessing. A verb the catalog never saw is
/// rejected before it reaches the server, so the whitelist is decided
/// here rather than by whatever the server happens to accept.
fn run_command(cx: &mut ToolCx, args: &Value, want: Class) -> Result<Value, RpcError> {
    let cmd = args
        .get("command")
        .ok_or_else(|| (INVALID_PARAMS, "missing 'command' (array of strings)".to_string()))?;
    let argv = strings(cmd, "command")?;
    if argv.is_empty() {
        return Err((INVALID_PARAMS, "'command' must be a non-empty array of strings".to_string()));
    }
    let verb = argv[0].to_ascii_uppercase();
    match (cx.catalog.classify(&verb), want) {
        (None, _) => Err((
            INVALID_PARAMS,
            format!("unknown verb '{verb}' — call kevy_discover for the full verb table"),
        )),
        (Some(Class::Write), Class::Read) => {
            Err((INVALID_PARAMS, format!("'{verb}' is a write verb — use kevy_write")))
        }
        (Some(Class::Read), Class::Write) => {
            Err((INVALID_PARAMS, format!("'{verb}' is read-only — use kevy_read")))
        }
        _ => {
            let argv: Vec<Vec<u8>> = argv.iter().map(|a| a.as_bytes().to_vec()).collect();
            Ok(tool_result(&request(cx, &argv)?))
        }
    }
}

/// Passes through to `IDX.EXPLAIN <index> [args…]`.
fn explain(cx: &mut ToolCx, args: &Value) -> Result<Value, RpcError> {
    let index = args
        .get("index")
        .and_then(Value::as_str)
        .ok_or_else(|| (INVALID_PARAMS, "missing 'index' (string)".to_string()))?;
    let mut argv = vec![b"IDX.EXPLAIN".to_vec(), index.as_bytes().to_vec()];
    if let Some(extra) = args.get("args") {
        for a in strings(extra, "args")? {
            argv.push(a.into_bytes());
        }
    }
    Ok(tool_result(&request(cx, &argv)?))
}

/// Runs `INFO [section]`.
fn info(cx: &mut ToolCx, args: &Value) -> Result<Value, RpcError> {
    let mut argv = vec![b"INFO".to_vec()];
    if let Some(section) = args.get("section").and_then(Value::as_str) {
        argv.push(section.as_bytes().to_vec());
    }
    Ok(tool_result(&request(cx, &argv)?))
}

/// One round trip, with a transport failure mapped to `-32603`.
///
/// A lost connection is an internal error rather than a tool result: the
/// answer is not "the server said no", it is that there was no answer.
fn request(cx: &mut ToolCx, argv: &[Vec<u8>]) -> Result<Reply, RpcError> {
    cx.client.request(argv).map_err(|e| (INTERNAL, format!("kevy request failed: {e}")))
}

/// An argument that must be an array of strings, or the error saying so.
///
/// `what` names the field, so the message points at `command` or `args`
/// rather than at "an array".
fn strings(v: &Value, what: &str) -> Result<Vec<String>, RpcError> {
    let arr = v
        .as_array()
        .ok_or_else(|| (INVALID_PARAMS, format!("'{what}' must be an array of strings")))?;
    arr.iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| (INVALID_PARAMS, format!("'{what}' items must all be strings")))
        })
        .collect()
}

/// Shape a RESP reply as an MCP tool result. Error replies keep the raw
/// server text verbatim — including the `ERR`/`WRONGTYPE` code prefix —
/// and set `isError:true`; everything else is JSON-ified.
pub fn tool_result(reply: &Reply) -> Value {
    match reply {
        Reply::Error(e) | Reply::BlobError(e) => {
            text_result(String::from_utf8_lossy(e).into_owned(), true)
        }
        other => text_result(reply_to_json(other).serialize(), false),
    }
}

/// An MCP tool result carrying one block of text.
fn text_result(text: String, is_error: bool) -> Value {
    obj(vec![
        ("content", Value::Array(vec![obj(vec![("type", s("text")), ("text", Value::Str(text))])])),
        ("isError", Value::Bool(is_error)),
    ])
}

/// Recursively JSON-ify a RESP reply. Binary payloads go through
/// `from_utf8_lossy` — MCP text content is UTF-8 by definition.
pub fn reply_to_json(reply: &Reply) -> Value {
    match reply {
        Reply::Simple(b) | Reply::Bulk(b) | Reply::BigNumber(b) => lossy(b),
        Reply::Verbatim { data, .. } => lossy(data),
        Reply::Error(e) | Reply::BlobError(e) => obj(vec![("error", lossy(e))]),
        Reply::Int(n) => Value::Int(*n),
        Reply::Double(d) if d.is_finite() => Value::Float(*d),
        // inf/-inf/nan — JSON has no such numbers; carry as strings.
        Reply::Double(d) => Value::Str(d.to_string()),
        Reply::Boolean(b) => Value::Bool(*b),
        Reply::Nil | Reply::Null => Value::Null,
        Reply::Array(items) | Reply::Set(items) | Reply::Push(items) => {
            Value::Array(items.iter().map(reply_to_json).collect())
        }
        Reply::Map(pairs) => {
            Value::Object(pairs.iter().map(|(k, v)| (key_text(k), reply_to_json(v))).collect())
        }
    }
}

/// Bytes as a JSON string, replacing anything that is not UTF-8.
fn lossy(b: &[u8]) -> Value {
    Value::Str(String::from_utf8_lossy(b).into_owned())
}

/// JSON object keys must be strings; stringify whatever the map key is.
fn key_text(k: &Reply) -> String {
    match k {
        Reply::Simple(b) | Reply::Bulk(b) => String::from_utf8_lossy(b).into_owned(),
        Reply::Int(n) => n.to_string(),
        other => reply_to_json(other).serialize(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_list_gates_kevy_write() {
        let names = |v: &Value| -> Vec<String> {
            v.get("tools")
                .and_then(Value::as_array)
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(|t| t.get("name").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        let ro = names(&tools_list(false));
        assert_eq!(ro, ["kevy_discover", "kevy_read", "kevy_explain", "kevy_info"]);
        let rw = names(&tools_list(true));
        assert!(rw.contains(&"kevy_write".to_string()));
        assert_eq!(rw.len(), 5);
    }

    #[test]
    fn every_tool_has_description_and_schema() {
        let list = tools_list(true);
        for t in list.get("tools").and_then(Value::as_array).into_iter().flatten() {
            assert!(t.get("description").and_then(Value::as_str).is_some());
            let schema = t.get("inputSchema").expect("inputSchema present");
            assert_eq!(schema.get("type").and_then(Value::as_str), Some("object"));
        }
    }

    #[test]
    fn reply_to_json_covers_the_resp_model() {
        let reply = Reply::Array(vec![
            Reply::Simple(b"OK".to_vec()),
            Reply::Bulk(b"v".to_vec()),
            Reply::Int(-3),
            Reply::Nil,
            Reply::Boolean(true),
            Reply::Double(1.5),
            Reply::Double(f64::INFINITY),
            Reply::Map(vec![(Reply::Bulk(b"k".to_vec()), Reply::Int(1))]),
        ]);
        assert_eq!(
            reply_to_json(&reply).serialize(),
            r#"["OK","v",-3,null,true,1.5,"inf",{"k":1}]"#
        );
    }

    #[test]
    fn error_reply_keeps_prefix_and_sets_is_error() {
        let r = tool_result(&Reply::Error(b"ERR unknown command 'NOPE'".to_vec()));
        assert_eq!(r.get("isError"), Some(&Value::Bool(true)));
        let text = r
            .get("content")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str);
        assert_eq!(text, Some("ERR unknown command 'NOPE'"));
    }

    #[test]
    fn ok_reply_is_json_text_content() {
        let r = tool_result(&Reply::Simple(b"PONG".to_vec()));
        assert_eq!(r.get("isError"), Some(&Value::Bool(false)));
        let text = r
            .get("content")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str);
        assert_eq!(text, Some("\"PONG\""));
    }
}

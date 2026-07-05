//! JSON-RPC 2.0 framing over newline-delimited stdio (the MCP stdio
//! transport): one JSON document per line, requests in on stdin,
//! responses out on stdout.

use crate::json::{self, Value, obj, s};

/// One decoded JSON-RPC message. `id: None` means notification — the
/// server must never answer it (JSON-RPC 2.0 §4.1).
pub struct Request {
    /// Echoed verbatim into the response (number, string — any JSON).
    pub id: Option<Value>,
    /// Method name, e.g. `tools/call`.
    pub method: String,
    /// `params` member; [`Value::Null`] when absent.
    pub params: Value,
}

/// Framing-level failures, each mapped to its JSON-RPC error code.
pub enum FrameError {
    /// Not valid JSON → `-32700`.
    Parse(String),
    /// Valid JSON but not a JSON-RPC request → `-32600`.
    Invalid(String),
}

/// Decode one stdin line into a [`Request`].
pub fn parse_request(line: &str) -> Result<Request, FrameError> {
    let doc = json::parse(line).map_err(FrameError::Parse)?;
    if !matches!(doc, Value::Object(_)) {
        return Err(FrameError::Invalid("request must be a JSON object".into()));
    }
    let method = match doc.get("method").and_then(Value::as_str) {
        Some(m) => m.to_string(),
        None => return Err(FrameError::Invalid("missing 'method' string".into())),
    };
    let id = doc.get("id").cloned();
    let params = doc.get("params").cloned().unwrap_or(Value::Null);
    Ok(Request { id, method, params })
}

/// Success response line: `{"jsonrpc":"2.0","id":…,"result":…}`.
pub fn ok_line(id: &Value, result: Value) -> String {
    obj(vec![
        ("jsonrpc", s("2.0")),
        ("id", id.clone()),
        ("result", result),
    ])
    .serialize()
}

/// Error response line: `{"jsonrpc":"2.0","id":…,"error":{code,message}}`.
pub fn err_line(id: &Value, code: i64, message: &str) -> String {
    obj(vec![
        ("jsonrpc", s("2.0")),
        ("id", id.clone()),
        (
            "error",
            obj(vec![("code", Value::Int(code)), ("message", s(message))]),
        ),
    ])
    .serialize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_with_id_and_params() {
        let r = parse_request(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{"a":1}}"#)
            .unwrap_or_else(|_| panic!("should parse"));
        assert_eq!(r.id, Some(Value::Int(7)));
        assert_eq!(r.method, "tools/list");
        assert_eq!(r.params.get("a"), Some(&Value::Int(1)));
    }

    #[test]
    fn notification_has_no_id() {
        let r = parse_request(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .unwrap_or_else(|_| panic!("should parse"));
        assert!(r.id.is_none());
        assert_eq!(r.params, Value::Null);
    }

    #[test]
    fn bad_json_is_parse_error() {
        assert!(matches!(parse_request("{nope"), Err(FrameError::Parse(_))));
    }

    #[test]
    fn missing_method_is_invalid_request() {
        assert!(matches!(
            parse_request(r#"{"jsonrpc":"2.0","id":1}"#),
            Err(FrameError::Invalid(_))
        ));
        assert!(matches!(
            parse_request("[1,2,3]"),
            Err(FrameError::Invalid(_))
        ));
    }

    #[test]
    fn response_lines_are_single_line_json() {
        let ok = ok_line(&Value::Int(1), s("done"));
        assert_eq!(ok, r#"{"jsonrpc":"2.0","id":1,"result":"done"}"#);
        let err = err_line(&Value::Null, -32601, "method not found: 'x'");
        assert_eq!(
            err,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32601,"message":"method not found: 'x'"}}"#
        );
    }
}

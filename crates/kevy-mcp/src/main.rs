//! kevy-mcp — kevy's official MCP (Model Context Protocol) server.
//!
//! Speaks the MCP stdio transport: newline-delimited JSON-RPC 2.0 on
//! stdin/stdout (protocol revision = the `protocolVersion` literal in
//! the `initialize` reply below; tools capability only).
//! On startup it connects to a kevy server and bootstraps its verb
//! catalog from `COMMAND DOCS` — the tool surface is defined by the
//! live server, never by a table baked into this binary.
//!
//! ```text
//! kevy-mcp [--url redis://127.0.0.1:6004] [--allow-writes]
//! ```
//!
//! Write verbs are opt-in: without `--allow-writes` the `kevy_write`
//! tool is not advertised and calls to it are rejected. Exits cleanly
//! on stdin EOF.

/// The verb surface, read from the live server rather than baked in here.
mod catalog;
/// A JSON reader and writer, because the workspace takes no dependencies.
mod json;
/// JSON-RPC 2.0 framing: requests in, one line of response out.
mod proto;
/// The five MCP tools and what each one is permitted to do.
mod tools;

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use kevy_resp_client::RespClient;

use catalog::Catalog;
use json::{Value, obj, s};
use proto::FrameError;
use tools::ToolCx;

/// The one-line usage text, printed for `--help` and beside any argument error.
const USAGE: &str = "usage: kevy-mcp [--url redis://127.0.0.1:6004] [--allow-writes]";

/// Everything the command line decides.
struct Cli {
    /// Where the kevy server is. The catalog is bootstrapped from it, so
    /// this also decides which verbs exist.
    url: String,
    /// Whether the `kevy_write` tool is advertised at all. Off by default:
    /// an agent that cannot see the tool cannot be talked into calling it.
    allow_writes: bool,
}

/// Reads the arguments, or names the first one it could not.
///
/// An unknown argument is an error rather than something skipped — this
/// binary is launched by an agent host from a config file, where a silently
/// ignored flag would mean the operator believes a setting is in force
/// when it is not. `--allow-writes` is exactly such a setting in reverse.
fn parse_cli(args: &[String]) -> Result<Cli, String> {
    let mut url = "redis://127.0.0.1:6004".to_string();
    let mut allow_writes = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--url" => url = it.next().ok_or("--url requires a value")?.clone(),
            "--allow-writes" => allow_writes = true,
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    Ok(Cli { url, allow_writes })
}

/// Parses the command line and runs the server until stdin reaches EOF.
///
/// Exit 2 for a bad argument, 1 for a failure while serving, 0 for a clean
/// EOF — the host distinguishes "you launched me wrong" from "I lost the
/// server", and neither looks like a normal shutdown.
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let cli = match parse_cli(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kevy-mcp: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kevy-mcp: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Connects, bootstraps the catalog from `COMMAND DOCS`, then serves.
///
/// The catalog is read before the first frame is answered, so `tools/list`
/// describes the verbs this particular server has rather than the ones
/// this binary was compiled against.
fn run(cli: &Cli) -> io::Result<()> {
    let mut client = RespClient::connect_url(&cli.url)?;
    let docs = client.request(&[b"COMMAND".to_vec(), b"DOCS".to_vec()])?;
    let catalog = Catalog::from_docs_reply(&docs).map_err(io::Error::other)?;
    // stderr is the MCP-sanctioned log channel; stdout carries only frames.
    eprintln!(
        "kevy-mcp: connected to {} — {} readonly / {} write verbs (writes {})",
        cli.url,
        catalog.read_count(),
        catalog.write_count(),
        if cli.allow_writes { "enabled" } else { "disabled" },
    );
    serve(&mut client, &catalog, cli.allow_writes)
}

/// stdin line loop: one JSON-RPC frame per line until EOF.
fn serve(client: &mut RespClient, catalog: &Catalog, allow_writes: bool) -> io::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let mut cx = ToolCx { client, catalog, allow_writes };
        if let Some(frame) = handle_line(&line, &mut cx) {
            stdout.write_all(frame.as_bytes())?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// One inbound line → at most one outbound frame (notifications get none).
fn handle_line(line: &str, cx: &mut ToolCx) -> Option<String> {
    let req = match proto::parse_request(line) {
        Ok(r) => r,
        Err(FrameError::Parse(m)) => return Some(proto::err_line(&Value::Null, -32700, &m)),
        Err(FrameError::Invalid(m)) => return Some(proto::err_line(&Value::Null, -32600, &m)),
    };
    let outcome = dispatch(&req, cx);
    let id = req.id?; // notification: never answered (JSON-RPC 2.0)
    Some(match outcome {
        Ok(result) => proto::ok_line(&id, result),
        Err((code, msg)) => proto::err_line(&id, code, &msg),
    })
}

/// Routes one parsed request to the method that answers it.
///
/// An unknown method is JSON-RPC's -32601 rather than a crash: a host may
/// probe for capabilities this revision does not implement, and the reply
/// that says so is part of the protocol.
fn dispatch(req: &proto::Request, cx: &mut ToolCx) -> Result<Value, (i64, String)> {
    match req.method.as_str() {
        "initialize" => Ok(initialize_result()),
        "notifications/initialized" => Ok(Value::Null),
        "ping" => Ok(Value::Object(Vec::new())),
        "tools/list" => Ok(tools::tools_list(cx.allow_writes)),
        "tools/call" => {
            let name = req
                .params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| (tools::INVALID_PARAMS, "missing tool 'name'".to_string()))?;
            let empty = Value::Object(Vec::new());
            let args = req.params.get("arguments").unwrap_or(&empty);
            tools::call_tool(cx, name, args)
        }
        other => Err((-32601, format!("method not found: '{other}'"))),
    }
}

/// The `initialize` reply: protocol revision, capabilities, and identity.
///
/// The version is `CARGO_PKG_VERSION`, so the handshake cannot report a
/// release this binary is not — one of the six layers a version lives in.
fn initialize_result() -> Value {
    obj(vec![
        ("protocolVersion", s("2024-11-05")),
        ("capabilities", obj(vec![("tools", obj(Vec::new()))])),
        (
            "serverInfo",
            obj(vec![("name", s("kevy-mcp")), ("version", s(env!("CARGO_PKG_VERSION")))]),
        ),
    ])
}
